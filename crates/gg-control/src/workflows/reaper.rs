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

/// Send again the tasks whose outputs the job's lab node did not pull before their deadline
/// (ADR-0041): the submission is closed as inconclusive by the server (not paid, nobody is
/// sanctioned) and the task is queued again while attempts remain. Returns how many.
pub async fn reissue_unpulled(state: &AppState) -> Result<usize, ApiError> {
    let mut c = state.db().pool().acquire().await?;
    let overdue = gg_storage::queries::pending::unpulled_overdue(&mut c, 50).await?;
    drop(c);
    let mut n = 0;
    for attempt in overdue {
        if reissue_unpulled_attempt(state, AttemptId(attempt), "deadline").await? {
            n += 1;
        }
    }
    Ok(n)
}

/// Close one submitted attempt whose outputs its job's lab node does not hold, as inconclusive
/// (not paid, nobody sanctioned), and queue its task again while attempts remain. `why`: the
/// deadline passed, or the node gave up pulling them. Returns false when the attempt is no longer
/// submitted.
pub async fn reissue_unpulled_attempt(
    state: &AppState,
    attempt: AttemptId,
    why: &str,
) -> Result<bool, ApiError> {
    let mut tx = state.db().begin().await?;
    // Task, then attempt: the order a decision takes its locks in.
    let Some(seen) = tasks::get_attempt(&mut tx, attempt).await? else {
        return Ok(false);
    };
    let Some(task) = jobs::lock_task(&mut tx, TaskId(seen.task_id)).await? else {
        return Ok(false);
    };
    let Some(a) = tasks::lock_attempt(&mut tx, attempt).await? else {
        return Ok(false);
    };
    if a.attempt_state()
        .map_err(|e| ApiError::Internal(e.to_string()))?
        != AttemptState::Submitted
    {
        return Ok(false);
    }
    let Some(sub) = tasks::get_submission_by_attempt(&mut tx, attempt).await? else {
        return Ok(false);
    };
    let mut reason = b"outputs-not-pulled:".to_vec();
    reason.extend_from_slice(a.id.as_bytes());
    let (next, _) = super::decide::finalize(
        state,
        &mut tx,
        &task,
        &a,
        &sub,
        VerificationOutcome::Inconclusive,
        Sha256Digest::of_bytes(&reason),
        &[],
        TaskEvent::Decide(VerificationOutcome::Inconclusive),
        Authority::Server,
    )
    .await?;
    tasks::close_verifications_of_attempt(&mut tx, attempt).await?;
    gg_storage::queries::pending::drop_attempt(&mut tx, a.id).await?;
    misc::outbox_insert(
        &mut tx,
        "attempt.outputs_not_pulled",
        &serde_json::json!({ "task_id": task.id, "attempt_id": a.id, "task_state": next, "why": why }),
    )
    .await?;
    tx.commit().await?;
    tracing::warn!(task_id = %task.id, attempt_id = %a.id, next_state = %next, why, "the lab node does not hold these outputs: task sent again");
    state.signal_work();
    Ok(true)
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
                if let Err(e) = reissue_unpulled(&state).await {
                    tracing::warn!(error = %e, "reissuing unpulled outputs failed");
                }
            }
            _ = shutdown.changed() => { if *shutdown.borrow() { break; } }
        }
    }
}
