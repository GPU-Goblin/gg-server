//! Messages of the reward distribution contract (ADR-0015).

use cosmwasm_schema::{QueryResponses, cw_serde};
use cosmwasm_std::Uint128;
use gg_merkle::{ProofStep, RewardLeafV1};
use serde::{Deserialize, Serialize};

/// Set at instantiation. The authority is the administered governance that publishes batches; it
/// attests an off-chain acceptance, it does not prove it.
#[cw_serde]
pub struct InstantiateMsg {
    /// Address allowed to publish batches, pause and hand over authority.
    pub authority: String,
    /// Denomination distributed. A test denomination in this phase: no real value is promised.
    pub denom: String,
    /// Platform network identifier committed in every leaf.
    pub network: String,
}

/// Actions. Plain serde rather than `cw_serde`: the committed leaf comes from `gg-merkle`, the crate
/// shared with the control plane, and it carries no JSON-schema derive.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ExecuteMsg {
    /// Commit an immutable list of rewards. The root cannot be replaced afterwards, and the
    /// contract must already hold enough funds to honour every open batch.
    PublishBatch {
        /// Batch identifier, unique for this contract.
        batch_id: String,
        /// Merkle root (hex) computed by `gg-merkle`.
        merkle_root: String,
        /// Sum of the committed amounts.
        total_amount: Uint128,
        /// Number of leaves, for auditability.
        leaf_count: u32,
    },
    /// Claim one reward. Anyone may relay the transaction; the funds go to the beneficiary named in
    /// the committed leaf.
    Claim {
        /// The committed leaf.
        leaf: Box<RewardLeafV1>,
        /// Proof from the leaf to the batch root.
        proof: Vec<ProofStep>,
    },
    /// Stop publications and claims (incident, migration).
    SetPaused {
        /// New state.
        paused: bool,
    },
    /// Hand authority to another address. Audited through an event.
    SetAuthority {
        /// New authority.
        authority: String,
    },
}

/// Queries.
#[cw_serde]
#[derive(QueryResponses)]
pub enum QueryMsg {
    /// Contract configuration and committed totals.
    #[returns(ConfigResponse)]
    Config {},
    /// One batch.
    #[returns(BatchResponse)]
    Batch {
        /// Identifier.
        batch_id: String,
    },
    /// Whether an obligation was already paid, whatever the batch it came from.
    #[returns(ClaimedResponse)]
    Claimed {
        /// Global reward identifier.
        reward_id: String,
    },
}

/// Configuration and accounting.
#[cw_serde]
pub struct ConfigResponse {
    /// Authority.
    pub authority: String,
    /// Denomination.
    pub denom: String,
    /// Platform network.
    pub network: String,
    /// Paused.
    pub paused: bool,
    /// Sum committed by every published batch.
    pub committed: Uint128,
    /// Sum already paid out.
    pub claimed: Uint128,
}

/// A published batch.
#[cw_serde]
pub struct BatchResponse {
    /// Identifier.
    pub batch_id: String,
    /// Root (hex).
    pub merkle_root: String,
    /// Committed total.
    pub total_amount: Uint128,
    /// Amount already claimed from this batch.
    pub claimed_amount: Uint128,
    /// Leaves committed.
    pub leaf_count: u32,
    /// Block height of publication.
    pub published_at: u64,
}

/// Whether an obligation was paid.
#[cw_serde]
pub struct ClaimedResponse {
    /// True when the reward id was already paid.
    pub claimed: bool,
    /// Batch it was paid from.
    pub batch_id: Option<String>,
}
