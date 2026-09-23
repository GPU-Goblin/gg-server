//! Outbox dispatcher: delivers events (to logs in P1) after their transaction committed.

use gg_storage::queries::misc;

use crate::error::ApiError;
use crate::state::AppState;

/// Deliver up to 100 pending events.
pub async fn dispatch_once(state: &AppState) -> Result<usize, ApiError> {
    let mut tx = state.db().begin().await?;
    let events = misc::outbox_pending(&mut tx, 100).await?;
    if events.is_empty() {
        tx.rollback().await?;
        return Ok(0);
    }
    for e in &events {
        tracing::info!(topic = %e.topic, payload = %e.payload, "event");
    }
    let ids: Vec<i64> = events.iter().map(|e| e.id).collect();
    misc::outbox_mark_delivered(&mut tx, &ids).await?;
    tx.commit().await?;
    Ok(ids.len())
}

/// Run the dispatcher until `shutdown` resolves.
pub async fn run(state: AppState, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                if let Err(e) = dispatch_once(&state).await {
                    tracing::warn!(error = %e, "outbox dispatch failed");
                }
            }
            _ = shutdown.changed() => { if *shutdown.borrow() { break; } }
        }
    }
}
