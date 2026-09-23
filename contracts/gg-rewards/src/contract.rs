//! Reward distribution (ADR-0015).
//!
//! What this contract does: it holds test funds, records batches committed by an administered
//! authority, and pays the beneficiary named in a committed leaf, once.
//!
//! What it does **not** do, and must never be presented as doing: it does not verify that any
//! computation happened, nor that a result was correct. The acceptance is off-chain, signed by
//! validators, and the leaf merely references the receipt that attests it. A published root proves a
//! list was committed, nothing more.

use cosmwasm_std::{
    BankMsg, Binary, Coin, Deps, DepsMut, Env, MessageInfo, Response, StdResult, Uint128, coins,
    entry_point, to_json_binary,
};
use gg_merkle::{ProofStep, RewardLeafV1, verify_proof};

use crate::error::ContractError;
use crate::msg::{
    BatchResponse, ClaimedResponse, ConfigResponse, ExecuteMsg, InstantiateMsg, QueryMsg,
};
use crate::state::{BATCHES, Batch, CLAIMED, CONFIG, Config, TOTALS, Totals};

/// Contract name recorded for migrations.
const CONTRACT_NAME: &str = "gpu-goblin/gg-rewards";
/// Contract version recorded for migrations.
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Instantiate with the authority, the distributed denomination and the platform network.
#[entry_point]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    cw2::set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    let authority = deps.api.addr_validate(&msg.authority)?;
    if msg.denom.is_empty() || msg.network.is_empty() {
        return Err(ContractError::Malformed(
            "denom and network are required".into(),
        ));
    }
    CONFIG.save(
        deps.storage,
        &Config {
            authority: authority.clone(),
            denom: msg.denom.clone(),
            network: msg.network.clone(),
            paused: false,
        },
    )?;
    TOTALS.save(
        deps.storage,
        &Totals {
            committed: Uint128::zero(),
            claimed: Uint128::zero(),
        },
    )?;
    Ok(Response::new()
        .add_attribute("action", "instantiate")
        .add_attribute("authority", authority)
        .add_attribute("denom", msg.denom)
        .add_attribute("network", msg.network))
}

/// Execute.
#[entry_point]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::PublishBatch {
            batch_id,
            merkle_root,
            total_amount,
            leaf_count,
        } => publish_batch(
            deps,
            env,
            info,
            batch_id,
            merkle_root,
            total_amount,
            leaf_count,
        ),
        ExecuteMsg::Claim { leaf, proof } => claim(deps, env, info, *leaf, proof),
        ExecuteMsg::SetPaused { paused } => set_paused(deps, info, paused),
        ExecuteMsg::SetAuthority { authority } => set_authority(deps, info, authority),
    }
}

fn authority_only(deps: &DepsMut, info: &MessageInfo) -> Result<Config, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    if info.sender != config.authority {
        return Err(ContractError::Unauthorized);
    }
    Ok(config)
}

/// Commit a batch. The root is immutable, and the contract must hold enough to honour every open
/// batch: a batch that could not be paid is refused at publication rather than at claim time.
fn publish_batch(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    batch_id: String,
    merkle_root: String,
    total_amount: Uint128,
    leaf_count: u32,
) -> Result<Response, ContractError> {
    let config = authority_only(&deps, &info)?;
    if config.paused {
        return Err(ContractError::Paused);
    }
    if batch_id.is_empty() || batch_id.len() > 128 {
        return Err(ContractError::Malformed("batch_id length".into()));
    }
    if hex::decode(&merkle_root).map(|b| b.len()) != Ok(32) {
        return Err(ContractError::Malformed(
            "merkle_root must be 32 bytes of hex".into(),
        ));
    }
    if leaf_count == 0 || total_amount.is_zero() {
        return Err(ContractError::BadTotal);
    }
    if BATCHES.has(deps.storage, &batch_id) {
        return Err(ContractError::BatchExists(batch_id));
    }
    let mut totals = TOTALS.load(deps.storage)?;
    let committed = totals
        .committed
        .checked_add(total_amount)
        .map_err(|_| ContractError::BadTotal)?;
    // Solvency: what is owed and not yet paid must be covered by the balance held.
    let outstanding = committed
        .checked_sub(totals.claimed)
        .map_err(|_| ContractError::BadTotal)?;
    let balance = deps
        .querier
        .query_balance(env.contract.address.clone(), &config.denom)?;
    if balance.amount < cosmwasm_std::Uint256::from(outstanding) {
        return Err(ContractError::Underfunded {
            needed: outstanding.to_string(),
            balance: balance.amount.to_string(),
        });
    }
    totals.committed = committed;
    TOTALS.save(deps.storage, &totals)?;
    BATCHES.save(
        deps.storage,
        &batch_id,
        &Batch {
            merkle_root: merkle_root.clone(),
            total_amount,
            claimed_amount: Uint128::zero(),
            leaf_count,
            published_at: env.block.height,
        },
    )?;
    Ok(Response::new()
        .add_attribute("action", "publish_batch")
        .add_attribute("batch_id", batch_id)
        .add_attribute("merkle_root", merkle_root)
        .add_attribute("total_amount", total_amount)
        .add_attribute("leaf_count", leaf_count.to_string())
        .add_attribute("authority", info.sender))
}

/// Pay one committed reward to its beneficiary.
fn claim(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    leaf: RewardLeafV1,
    proof: Vec<ProofStep>,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    if config.paused {
        return Err(ContractError::Paused);
    }
    // The leaf must be committed to this contract, this chain, this network and this denomination.
    if leaf.contract != env.contract.address.as_str() {
        return Err(ContractError::WrongBinding("contract"));
    }
    if leaf.chain_id != env.block.chain_id {
        return Err(ContractError::WrongBinding("chain"));
    }
    if leaf.network != config.network {
        return Err(ContractError::WrongBinding("network"));
    }
    if leaf.asset != config.denom {
        return Err(ContractError::WrongBinding("asset"));
    }
    leaf.validate()
        .map_err(|e| ContractError::Malformed(e.to_string()))?;
    // One obligation, one payment, whatever batch it is presented from.
    if let Some(previous) = CLAIMED.may_load(deps.storage, &leaf.reward_id)? {
        let _ = previous;
        return Err(ContractError::AlreadyClaimed(leaf.reward_id));
    }
    let mut batch = BATCHES
        .may_load(deps.storage, &leaf.batch_id)?
        .ok_or_else(|| ContractError::UnknownBatch(leaf.batch_id.clone()))?;
    let root: [u8; 32] = hex::decode(&batch.merkle_root)
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
        .ok_or_else(|| ContractError::Malformed("stored root".into()))?;
    let ok =
        verify_proof(&leaf, &proof, &root).map_err(|e| ContractError::Malformed(e.to_string()))?;
    if !ok {
        return Err(ContractError::BadProof(leaf.batch_id));
    }
    let amount = Uint128::new(leaf.amount);
    let claimed_amount = batch
        .claimed_amount
        .checked_add(amount)
        .map_err(|_| ContractError::BadTotal)?;
    if claimed_amount > batch.total_amount {
        // The committed leaves would pay more than the batch announced: refuse rather than dip into
        // another batch's funds.
        return Err(ContractError::BadTotal);
    }
    batch.claimed_amount = claimed_amount;
    BATCHES.save(deps.storage, &leaf.batch_id, &batch)?;
    CLAIMED.save(deps.storage, &leaf.reward_id, &leaf.batch_id)?;
    let mut totals = TOTALS.load(deps.storage)?;
    totals.claimed = totals
        .claimed
        .checked_add(amount)
        .map_err(|_| ContractError::BadTotal)?;
    TOTALS.save(deps.storage, &totals)?;
    // The funds go to the committed beneficiary, never to the sender: a relay earns nothing.
    let beneficiary = deps.api.addr_validate(&leaf.beneficiary)?;
    let payment = BankMsg::Send {
        to_address: beneficiary.to_string(),
        amount: coins(amount.u128(), &config.denom),
    };
    Ok(Response::new()
        .add_message(payment)
        .add_attribute("action", "claim")
        .add_attribute("batch_id", leaf.batch_id)
        .add_attribute("reward_id", leaf.reward_id)
        .add_attribute("beneficiary", beneficiary)
        .add_attribute("relayer", info.sender)
        .add_attribute("amount", amount))
}

fn set_paused(deps: DepsMut, info: MessageInfo, paused: bool) -> Result<Response, ContractError> {
    let mut config = authority_only(&deps, &info)?;
    config.paused = paused;
    CONFIG.save(deps.storage, &config)?;
    Ok(Response::new()
        .add_attribute("action", "set_paused")
        .add_attribute("paused", paused.to_string())
        .add_attribute("authority", info.sender))
}

fn set_authority(
    deps: DepsMut,
    info: MessageInfo,
    authority: String,
) -> Result<Response, ContractError> {
    let mut config = authority_only(&deps, &info)?;
    let next = deps.api.addr_validate(&authority)?;
    let previous = config.authority.clone();
    config.authority = next.clone();
    CONFIG.save(deps.storage, &config)?;
    Ok(Response::new()
        .add_attribute("action", "set_authority")
        .add_attribute("previous", previous)
        .add_attribute("authority", next))
}

/// Queries.
#[entry_point]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => {
            let config = CONFIG.load(deps.storage)?;
            let totals = TOTALS.load(deps.storage)?;
            to_json_binary(&ConfigResponse {
                authority: config.authority.to_string(),
                denom: config.denom,
                network: config.network,
                paused: config.paused,
                committed: totals.committed,
                claimed: totals.claimed,
            })
        }
        QueryMsg::Batch { batch_id } => {
            let b = BATCHES.load(deps.storage, &batch_id)?;
            to_json_binary(&BatchResponse {
                batch_id,
                merkle_root: b.merkle_root,
                total_amount: b.total_amount,
                claimed_amount: b.claimed_amount,
                leaf_count: b.leaf_count,
                published_at: b.published_at,
            })
        }
        QueryMsg::Claimed { reward_id } => {
            let batch_id = CLAIMED.may_load(deps.storage, &reward_id)?;
            to_json_binary(&ClaimedResponse {
                claimed: batch_id.is_some(),
                batch_id,
            })
        }
    }
}

/// Coins helper used by tests and tooling.
pub fn denom_coin(amount: u128, denom: &str) -> Coin {
    Coin::new(amount, denom)
}
