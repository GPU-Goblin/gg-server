//! Lease reaper: expired attempts lose their lease; tasks are re-queued with a new generation or fail.

use gg_core::*;
use gg_storage::queries::{jobs, misc, tasks};

use crate::error::ApiError;
use crate::state::AppState;

/// Expire up to 50 attempts. Returns how many were processed.
pub async fn reap_once(state: &AppState) -> Result<usize, ApiError> {
    let mut tx = state.db().begin().await?;
    let expired = tasks::expired_active_attempts(&mut tx, 50).await?;
    let mut n = 0;
    for a in expired {
        let Some(task) = jobs::lock_task(&mut tx, TaskId(a.task_id)).await? else {
            continue;
        };
        let a_state = a
            .attempt_state()
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        tasks::update_attempt_state(
            &mut tx,
            AttemptId(a.id),
            a_state.next(AttemptEvent::Expire)?,
        )
        .await?;
        tasks::release_lease(&mut tx, AttemptId(a.id)).await?;
        gg_ledger::release(&mut tx, AttemptId(a.id)).await?;
        let t_state = task
            .task_state()
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        let mut next = t_state;
        if t_state == TaskState::Assigned && task.generation == a.generation {
            next = t_state.next(
                TaskEvent::LeaseLost {
                    attempts_left: super::attempts_left(task.attempts_made, task.max_attempts),
                },
                Authority::Server,
            )?;
            jobs::update_task_state(&mut tx, TaskId(task.id), next).await?;
        }
        misc::outbox_insert(
            &mut tx,
            "attempt.expired",
            &serde_json::json!({ "task_id": task.id, "attempt_id": a.id, "worker_id": a.worker_id, "task_state": next }),
        )
        .await?;
        tracing::info!(task_id = %task.id, attempt_id = %a.id, next_state = %next, "lease expired");
        n += 1;
    }
    let expired_peers = gg_storage::queries::p2p::delete_expired_peers(&mut tx).await?;
    if expired_peers > 0 {
        tracing::debug!(expired_peers, "peer announcements expired");
    }
    tx.commit().await?;
    if n > 0 {
        state.signal_work();
    }
    Ok(n)
}

/// Run the reaper until `shutdown` resolves.
pub async fn run(state: AppState, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let interval = std::time::Duration::from_secs(state.config().reaper_interval_secs.max(1));
    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {
                match reap_once(&state).await {
                    Ok(n) if n > 0 => tracing::info!(expired = n, "reaper pass"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "reaper pass failed"),
                }
            }
            _ = shutdown.changed() => { if *shutdown.borrow() { break; } }
        }
    }
}
