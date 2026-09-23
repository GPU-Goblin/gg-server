//! Stored state of the reward contract.

use cosmwasm_schema::cw_serde;
use cosmwasm_std::{Addr, Uint128};
use cw_storage_plus::{Item, Map};

/// Contract configuration.
#[cw_serde]
pub struct Config {
    /// Administered governance allowed to publish, pause and hand over.
    pub authority: Addr,
    /// Denomination distributed.
    pub denom: String,
    /// Platform network committed in every leaf.
    pub network: String,
    /// Whether publications and claims are stopped.
    pub paused: bool,
}

/// A published batch. Its root is written once and never changed.
#[cw_serde]
pub struct Batch {
    /// Merkle root (hex).
    pub merkle_root: String,
    /// Committed total.
    pub total_amount: Uint128,
    /// Already paid from this batch.
    pub claimed_amount: Uint128,
    /// Leaves committed.
    pub leaf_count: u32,
    /// Publication height.
    pub published_at: u64,
}

/// Accounting of what is owed and what was paid.
#[cw_serde]
pub struct Totals {
    /// Sum of every published batch.
    pub committed: Uint128,
    /// Sum already paid.
    pub claimed: Uint128,
}

/// Configuration.
pub const CONFIG: Item<Config> = Item::new("config");
/// Totals.
pub const TOTALS: Item<Totals> = Item::new("totals");
/// Batches by identifier.
pub const BATCHES: Map<&str, Batch> = Map::new("batches");
/// Obligations already paid, by global reward id. This map is what makes a second payment of the
/// same obligation impossible, including from another batch.
pub const CLAIMED: Map<&str, String> = Map::new("claimed");
