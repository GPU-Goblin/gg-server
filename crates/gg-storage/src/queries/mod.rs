//! Table-level query functions. Each takes `&mut PgConnection` so callers compose transactions.

pub mod artifacts;
pub mod collective;
pub mod contributors;
pub mod federated;
pub mod identities;
pub mod jobs;
pub mod ledger;
pub mod misc;
pub mod nodes;
pub mod p2p;
pub mod pending;
pub mod replicas;
pub mod rewards;
pub mod tasks;
