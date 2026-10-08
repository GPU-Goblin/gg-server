//! Transactional workflows. Each function opens one short transaction, calls the state machines of
//! `gg-core`, persists through `gg-storage`/`gg-ledger` and appends outbox events.

pub mod cancel;
pub mod claim;
pub mod collective;
pub mod contributors;
pub mod decide;
pub mod federated;
pub mod jobs;
pub mod login;
pub mod node_step;
pub mod outbox;
pub mod p2p;
pub mod push;
pub mod reaper;
pub mod relay;
pub mod replicas;
pub mod retention;
pub mod rewards;
pub mod staged;
pub mod step_cache;
pub mod submit;
pub mod withdraw;

use gg_core::*;
use gg_protocol::canonical;
use gg_protocol::messages::TaskSpecV1;

use crate::error::ApiError;

/// Decode a stored task spec.
pub fn decode_spec(bytes: &[u8]) -> Result<TaskSpecV1, ApiError> {
    canonical::from_bytes(bytes)
        .map_err(|e| ApiError::Internal(format!("stored spec undecodable: {e}")))
}

/// Server time in protocol milliseconds.
pub fn now_ms() -> UnixMillis {
    UnixMillis::now()
}

/// Whether more attempts may be scheduled after the current one.
pub fn attempts_left(attempts_made: i32, max_attempts: i32) -> bool {
    attempts_made < max_attempts
}

/// Run a transactional workflow, replaying it when the database aborted it to resolve a deadlock
/// or a serialization conflict (nothing was written then). Two concurrent workflows that lock the
/// same rows in a different order — a decision and the cancellation of a poisoned round, for
/// instance — are both served instead of one failing.
pub async fn retrying<T, F, Fut>(mut run: F) -> Result<T, crate::error::ApiError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, crate::error::ApiError>>,
{
    let mut attempt = 0u32;
    loop {
        match run().await {
            Err(e) if e.is_retryable() && attempt < 4 => {
                attempt += 1;
                tracing::debug!(attempt, error = %e, "transaction aborted by the database, replayed");
                tokio::time::sleep(std::time::Duration::from_millis(5 * u64::from(attempt))).await;
            }
            other => return other,
        }
    }
}
