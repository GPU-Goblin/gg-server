//! The reward contract on a simulated chain (`cw-multi-test`): real bank transfers, real storage,
//! real block context. What P7 must establish is checked here — a payment goes to the committed
//! beneficiary once, and every replay path is refused.
//!
//! The batches come from the **same** `gg-merkle` crate the control plane uses, and one test replays
//! the shared fixtures, so a drift between the off-chain commitment and the on-chain verification
//! breaks a test instead of a payment.

use cosmwasm_std::{Addr, Coin, Uint128, coins};
use cw_multi_test::{App, AppBuilder, ContractWrapper, Executor};
use gg_merkle::{Batch, ProofStep, RewardLeafV1};
use gg_rewards::msg::{
    BatchResponse, ClaimedResponse, ConfigResponse, ExecuteMsg, InstantiateMsg, QueryMsg,
};

const DENOM: &str = "utrg";
const NETWORK: &str = "gg-dev-local";
const CHAIN_ID: &str = "gg-localnet-1";

/// Addresses must be valid for the chain the contract runs on: the contract validates every
/// address it pays, so the tests use the same derivation as the simulated chain.
fn addr(name: &str) -> Addr {
    cosmwasm_std::testing::MockApi::default().addr_make(name)
}
fn authority() -> Addr {
    addr("authority")
}
fn alice() -> Addr {
    addr("alice")
}
fn bob() -> Addr {
    addr("bob")
}
fn relayer() -> Addr {
    addr("relayer")
}

fn app_with_funds() -> App {
    AppBuilder::new()
        .with_block(cosmwasm_std::BlockInfo {
            height: 100,
            time: cosmwasm_std::Timestamp::from_seconds(1_760_000_000),
            chain_id: CHAIN_ID.to_string(),
        })
        .build(|router, _, storage| {
            router
                .bank
                .init_balance(storage, &authority(), coins(1_000_000, DENOM))
                .unwrap();
        })
}

fn store_and_instantiate(app: &mut App) -> Addr {
    let code = ContractWrapper::new(
        gg_rewards::contract::execute,
        gg_rewards::contract::instantiate,
        gg_rewards::contract::query,
    );
    let code_id = app.store_code(Box::new(code));
    app.instantiate_contract(
        code_id,
        authority(),
        &InstantiateMsg {
            authority: authority().to_string(),
            denom: DENOM.to_string(),
            network: NETWORK.to_string(),
        },
        &[],
        "gg-rewards",
        None,
    )
    .unwrap()
}

/// Fund the contract like a treasury would before committing a batch.
fn fund(app: &mut App, contract: &Addr, amount: u128) {
    app.send_tokens(authority(), contract.clone(), &coins(amount, DENOM))
        .unwrap();
}

fn leaf(
    contract: &Addr,
    batch_id: &str,
    reward_id: &str,
    who: &Addr,
    amount: u128,
) -> RewardLeafV1 {
    RewardLeafV1 {
        network: NETWORK.into(),
        chain_id: CHAIN_ID.into(),
        contract: contract.to_string(),
        batch_id: batch_id.into(),
        reward_id: reward_id.into(),
        beneficiary: who.to_string(),
        asset: DENOM.into(),
        amount,
        receipt_hash: "ab".repeat(32),
    }
}

fn publish(app: &mut App, contract: &Addr, batch: &Batch, batch_id: &str) -> anyhow::Result<()> {
    app.execute_contract(
        authority(),
        contract.clone(),
        &ExecuteMsg::PublishBatch {
            batch_id: batch_id.to_string(),
            merkle_root: batch.root_hex(),
            total_amount: Uint128::new(batch.total_amount().unwrap()),
            leaf_count: batch.leaves().len() as u32,
        },
        &[],
    )
    .map(|_| ())
    .map_err(|e| anyhow::anyhow!("{e:#}"))
}

fn claim(
    app: &mut App,
    contract: &Addr,
    sender: Addr,
    leaf: &RewardLeafV1,
    proof: Vec<ProofStep>,
) -> anyhow::Result<()> {
    app.execute_contract(
        sender,
        contract.clone(),
        &ExecuteMsg::Claim {
            leaf: Box::new(leaf.clone()),
            proof,
        },
        &[],
    )
    .map(|_| ())
    .map_err(|e| anyhow::anyhow!("{e:#}"))
}

fn balance(app: &App, who: &Addr) -> u128 {
    let amount = app.wrap().query_balance(who, DENOM).unwrap().amount;
    Uint128::try_from(amount)
        .expect("test balances stay small")
        .u128()
}

fn proof_for(batch: &Batch, reward_id: &str) -> Vec<ProofStep> {
    batch.proof(batch.index_of(reward_id).unwrap()).unwrap()
}

#[test]
fn a_committed_reward_is_paid_once_to_its_beneficiary() {
    let mut app = app_with_funds();
    let contract = store_and_instantiate(&mut app);
    fund(&mut app, &contract, 100);

    let batch = Batch::build(vec![
        leaf(&contract, "batch-1", "reward-a", &alice(), 40),
        leaf(&contract, "batch-1", "reward-b", &bob(), 60),
    ])
    .unwrap();
    publish(&mut app, &contract, &batch, "batch-1").unwrap();

    // A third party relays the transaction; the funds go to the committed beneficiary, not to it.
    let alice_leaf = batch
        .leaves()
        .iter()
        .find(|l| l.reward_id == "reward-a")
        .unwrap();
    claim(
        &mut app,
        &contract,
        relayer(),
        alice_leaf,
        proof_for(&batch, "reward-a"),
    )
    .unwrap();
    assert_eq!(balance(&app, &alice()), 40);
    assert_eq!(balance(&app, &relayer()), 0, "a relayer earns nothing");
    assert_eq!(balance(&app, &contract), 60);

    // Claiming the same obligation again is refused.
    let err = claim(
        &mut app,
        &contract,
        alice(),
        alice_leaf,
        proof_for(&batch, "reward-a"),
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("already claimed"), "{err:#}");
    assert_eq!(balance(&app, &alice()), 40);

    let claimed: ClaimedResponse = app
        .wrap()
        .query_wasm_smart(
            &contract,
            &QueryMsg::Claimed {
                reward_id: "reward-a".to_string(),
            },
        )
        .unwrap();
    assert!(claimed.claimed);
    assert_eq!(claimed.batch_id.as_deref(), Some("batch-1"));

    // The second beneficiary is paid independently.
    let bob_leaf = batch
        .leaves()
        .iter()
        .find(|l| l.reward_id == "reward-b")
        .unwrap();
    claim(
        &mut app,
        &contract,
        bob(),
        bob_leaf,
        proof_for(&batch, "reward-b"),
    )
    .unwrap();
    assert_eq!(balance(&app, &bob()), 60);
    assert_eq!(balance(&app, &contract), 0);

    let config: ConfigResponse = app
        .wrap()
        .query_wasm_smart(&contract, &QueryMsg::Config {})
        .unwrap();
    assert_eq!(config.committed, Uint128::new(100));
    assert_eq!(config.claimed, Uint128::new(100));
}

#[test]
fn the_same_obligation_cannot_be_paid_from_two_batches() {
    let mut app = app_with_funds();
    let contract = store_and_instantiate(&mut app);
    fund(&mut app, &contract, 200);

    let first = Batch::build(vec![leaf(&contract, "batch-1", "reward-a", &alice(), 40)]).unwrap();
    publish(&mut app, &contract, &first, "batch-1").unwrap();
    // A second batch re-commits the same obligation: the leaf differs only by its batch id.
    let second = Batch::build(vec![leaf(&contract, "batch-2", "reward-a", &alice(), 40)]).unwrap();
    publish(&mut app, &contract, &second, "batch-2").unwrap();

    claim(
        &mut app,
        &contract,
        relayer(),
        &first.leaves()[0],
        proof_for(&first, "reward-a"),
    )
    .unwrap();
    let err = claim(
        &mut app,
        &contract,
        relayer(),
        &second.leaves()[0],
        proof_for(&second, "reward-a"),
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("already claimed"), "{err:#}");
    assert_eq!(balance(&app, &alice()), 40, "paid once, not twice");
}

#[test]
fn a_proof_of_another_batch_contract_or_chain_is_refused() {
    let mut app = app_with_funds();
    let contract = store_and_instantiate(&mut app);
    fund(&mut app, &contract, 200);

    let batch = Batch::build(vec![
        leaf(&contract, "batch-1", "reward-a", &alice(), 40),
        leaf(&contract, "batch-1", "reward-b", &bob(), 60),
    ])
    .unwrap();
    publish(&mut app, &contract, &batch, "batch-1").unwrap();
    let alice_leaf = batch
        .leaves()
        .iter()
        .find(|l| l.reward_id == "reward-a")
        .unwrap();

    // The proof of another leaf does not commit this one.
    let err = claim(
        &mut app,
        &contract,
        relayer(),
        alice_leaf,
        proof_for(&batch, "reward-b"),
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("does not commit"), "{err:#}");

    // A leaf committed to another chain, contract or network is refused before any proof work.
    for (field, mutate) in [
        (
            "chain",
            Box::new(|l: &mut RewardLeafV1| l.chain_id = "other-1".into())
                as Box<dyn Fn(&mut RewardLeafV1)>,
        ),
        (
            "contract",
            Box::new(|l: &mut RewardLeafV1| l.contract = addr("other-contract").to_string()),
        ),
        (
            "network",
            Box::new(|l: &mut RewardLeafV1| l.network = "gg-other".into()),
        ),
        (
            "asset",
            Box::new(|l: &mut RewardLeafV1| l.asset = "uatom".into()),
        ),
    ] {
        let mut tampered = alice_leaf.clone();
        mutate(&mut tampered);
        let err = claim(
            &mut app,
            &contract,
            relayer(),
            &tampered,
            proof_for(&batch, "reward-a"),
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains(&format!("another {field}")),
            "{field}: {err:#}"
        );
    }

    // Raising the amount breaks the commitment.
    let mut richer = alice_leaf.clone();
    richer.amount = 1_000;
    let err = claim(
        &mut app,
        &contract,
        relayer(),
        &richer,
        proof_for(&batch, "reward-a"),
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("does not commit"), "{err:#}");

    // Redirecting the payment breaks it too.
    let mut redirected = alice_leaf.clone();
    redirected.beneficiary = relayer().to_string();
    let err = claim(
        &mut app,
        &contract,
        relayer(),
        &redirected,
        proof_for(&batch, "reward-a"),
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("does not commit"), "{err:#}");
    assert_eq!(balance(&app, &relayer()), 0);
    assert_eq!(
        balance(&app, &alice()),
        0,
        "nothing was paid by any attempt"
    );
}

#[test]
fn a_published_root_is_immutable_and_a_batch_must_be_funded() {
    let mut app = app_with_funds();
    let contract = store_and_instantiate(&mut app);

    let batch = Batch::build(vec![leaf(&contract, "batch-1", "reward-a", &alice(), 40)]).unwrap();
    // Nothing funded yet: the batch could not be honoured, so it is refused now rather than later.
    let err = publish(&mut app, &contract, &batch, "batch-1").unwrap_err();
    assert!(format!("{err:#}").contains("underfunded"), "{err:#}");

    fund(&mut app, &contract, 40);
    publish(&mut app, &contract, &batch, "batch-1").unwrap();

    // Re-publishing the same identifier with another root is refused.
    let other = Batch::build(vec![leaf(&contract, "batch-1", "reward-z", &bob(), 1)]).unwrap();
    let err = publish(&mut app, &contract, &other, "batch-1").unwrap_err();
    assert!(format!("{err:#}").contains("cannot be replaced"), "{err:#}");
    let stored: BatchResponse = app
        .wrap()
        .query_wasm_smart(
            &contract,
            &QueryMsg::Batch {
                batch_id: "batch-1".to_string(),
            },
        )
        .unwrap();
    assert_eq!(stored.merkle_root, batch.root_hex());

    // A second batch is refused while the first is not covered by the remaining balance.
    let second = Batch::build(vec![leaf(&contract, "batch-2", "reward-b", &bob(), 60)]).unwrap();
    let err = publish(&mut app, &contract, &second, "batch-2").unwrap_err();
    assert!(format!("{err:#}").contains("underfunded"), "{err:#}");
    fund(&mut app, &contract, 60);
    publish(&mut app, &contract, &second, "batch-2").unwrap();
}

#[test]
fn only_the_authority_publishes_and_a_pause_stops_everything() {
    let mut app = app_with_funds();
    let contract = store_and_instantiate(&mut app);
    fund(&mut app, &contract, 100);
    let batch = Batch::build(vec![leaf(&contract, "batch-1", "reward-a", &alice(), 40)]).unwrap();

    // Anyone else publishing is refused.
    let err = app
        .execute_contract(
            alice(),
            contract.clone(),
            &ExecuteMsg::PublishBatch {
                batch_id: "batch-1".into(),
                merkle_root: batch.root_hex(),
                total_amount: Uint128::new(40),
                leaf_count: 1,
            },
            &[],
        )
        .unwrap_err();
    assert!(format!("{err:#}").contains("only the authority"), "{err:#}");
    publish(&mut app, &contract, &batch, "batch-1").unwrap();

    // Paused: no claim, no publication.
    app.execute_contract(
        authority(),
        contract.clone(),
        &ExecuteMsg::SetPaused { paused: true },
        &[],
    )
    .unwrap();
    let err = claim(
        &mut app,
        &contract,
        relayer(),
        &batch.leaves()[0],
        proof_for(&batch, "reward-a"),
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("paused"), "{err:#}");

    // Authority handover is recorded, and the old authority loses its powers.
    app.execute_contract(
        authority(),
        contract.clone(),
        &ExecuteMsg::SetAuthority {
            authority: bob().to_string(),
        },
        &[],
    )
    .unwrap();
    let err = app
        .execute_contract(
            authority(),
            contract.clone(),
            &ExecuteMsg::SetPaused { paused: false },
            &[],
        )
        .unwrap_err();
    assert!(format!("{err:#}").contains("only the authority"), "{err:#}");
    app.execute_contract(
        bob(),
        contract.clone(),
        &ExecuteMsg::SetPaused { paused: false },
        &[],
    )
    .unwrap();
    claim(
        &mut app,
        &contract,
        relayer(),
        &batch.leaves()[0],
        proof_for(&batch, "reward-a"),
    )
    .unwrap();
    assert_eq!(balance(&app, &alice()), 40);
}

/// The fixtures of `gg-merkle` are replayed through the contract: both sides agree on the encoding,
/// the ordering and the tree, or this test fails.
#[test]
fn the_shared_test_vectors_verify_on_chain() {
    let dir =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/merkle");
    let text = std::fs::read_to_string(dir.join("batch_even.json")).expect("shared fixture");
    let vector: serde_json::Value = serde_json::from_str(&text).unwrap();
    let leaves: Vec<RewardLeafV1> =
        serde_json::from_value(vector["leaves"].clone()).expect("fixture leaves");
    let batch = Batch::build(leaves).unwrap();
    assert_eq!(
        batch.root_hex(),
        vector["root"].as_str().unwrap(),
        "the fixture root must be reproduced before it is used on chain"
    );

    // Re-commit the fixture to this chain and contract, keeping the amounts and identifiers.
    let mut app = app_with_funds();
    let contract = store_and_instantiate(&mut app);
    let total: u128 = batch.total_amount().unwrap();
    fund(&mut app, &contract, total);
    let onchain: Vec<RewardLeafV1> = batch
        .leaves()
        .iter()
        .map(|l| RewardLeafV1 {
            network: NETWORK.into(),
            chain_id: CHAIN_ID.into(),
            contract: contract.to_string(),
            batch_id: "vectors-1".into(),
            asset: DENOM.into(),
            beneficiary: addr(&l.reward_id).to_string(),
            ..l.clone()
        })
        .collect();
    let rebuilt = Batch::build(onchain).unwrap();
    publish(&mut app, &contract, &rebuilt, "vectors-1").unwrap();
    for l in rebuilt.leaves() {
        let proof = proof_for(&rebuilt, &l.reward_id);
        claim(&mut app, &contract, relayer(), l, proof).unwrap();
        assert_eq!(
            balance(&app, &Addr::unchecked(l.beneficiary.clone())),
            l.amount
        );
    }
    assert_eq!(balance(&app, &contract), 0);
}

/// Bank balances are the ground truth: the sum paid never exceeds what was funded.
#[test]
fn the_contract_never_pays_more_than_it_holds() {
    let mut app = app_with_funds();
    let contract = store_and_instantiate(&mut app);
    fund(&mut app, &contract, 100);
    let batch = Batch::build(vec![
        leaf(&contract, "batch-1", "reward-a", &alice(), 60),
        leaf(&contract, "batch-1", "reward-b", &bob(), 40),
    ])
    .unwrap();
    publish(&mut app, &contract, &batch, "batch-1").unwrap();
    for id in ["reward-a", "reward-b"] {
        let l = batch.leaves().iter().find(|l| l.reward_id == id).unwrap();
        claim(&mut app, &contract, relayer(), l, proof_for(&batch, id)).unwrap();
    }
    assert_eq!(balance(&app, &alice()) + balance(&app, &bob()), 100);
    assert_eq!(balance(&app, &contract), 0);
    let config: ConfigResponse = app
        .wrap()
        .query_wasm_smart(&contract, &QueryMsg::Config {})
        .unwrap();
    assert_eq!(config.claimed, config.committed);
}

/// Sanity of the helper used by tooling.
#[test]
fn denom_coin_builds_the_expected_coin() {
    let c: Coin = gg_rewards::contract::denom_coin(7, DENOM);
    assert_eq!(c.denom, DENOM);
    assert_eq!(c.amount, cosmwasm_std::Uint256::from(7u128));
}
