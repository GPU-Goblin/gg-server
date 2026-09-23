//! GPU Goblin reward distribution contract (CosmWasm, phase P7, ADR-0015).
//!
//! **No real value.** This contract is exercised on a local test chain with a test denomination.
//! Nothing here authorizes issuing or commercializing an asset: that would require security audits,
//! a defined governance and a legal review, none of which exist yet (see docs/adr/0015).

#![forbid(unsafe_code)]

pub mod contract;
pub mod error;
pub mod msg;
pub mod state;

pub use crate::error::ContractError;
