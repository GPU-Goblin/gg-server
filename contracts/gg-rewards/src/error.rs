//! Errors of the reward contract. Each one names a refusal the specification requires.

use cosmwasm_std::StdError;
use thiserror::Error;

/// Refusals.
#[derive(Error, Debug)]
pub enum ContractError {
    /// Standard error.
    #[error("{0}")]
    Std(#[from] StdError),
    /// Only the authority may do this.
    #[error("only the authority may publish, pause or hand over authority")]
    Unauthorized,
    /// The contract is paused.
    #[error("the contract is paused")]
    Paused,
    /// A batch with this identifier exists: a published root is never replaced.
    #[error("batch {0} is already published and its root cannot be replaced")]
    BatchExists(String),
    /// Unknown batch.
    #[error("unknown batch {0}")]
    UnknownBatch(String),
    /// The contract does not hold enough funds for every open batch.
    #[error("batch underfunded: {needed} committed, balance {balance}")]
    Underfunded {
        /// Amount that would be committed in total.
        needed: String,
        /// Balance held.
        balance: String,
    },
    /// The proof does not reconstruct the batch root.
    #[error("the proof does not commit this leaf to batch {0}")]
    BadProof(String),
    /// The obligation was already paid.
    #[error("reward {0} was already claimed")]
    AlreadyClaimed(String),
    /// The leaf does not name this contract, chain or network.
    #[error("this leaf is committed to another {0}")]
    WrongBinding(&'static str),
    /// Malformed leaf or proof.
    #[error("malformed commitment: {0}")]
    Malformed(String),
    /// The declared total does not match the leaves, or overflows.
    #[error("the batch total is not usable")]
    BadTotal,
}
