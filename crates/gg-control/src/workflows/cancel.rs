//! Job cancellation: no further assignment, active attempts superseded, budget released.

use gg_core::*;
use gg_storage::queries::{jobs, misc, tasks};

use crate::error::ApiError;
use crate::state::AppState;

/// Cancel a job (idempotent). Returns the number of tasks moved to `cancelled`.
pub async fn cancel_job(
    state: &AppState,
    actor: &str,
    authority: Authority,
    job: JobId,
) -> Result<usize, ApiError> {
    let mut tx = state.db().begin().await?;
    let changed = cancel_job_in(&mut tx, actor, authority, job).await?;
    tx.commit().await?;
    Ok(changed)
}

/// Cancel a job inside the caller's transaction: the cancellation takes effect exactly when what
/// motivated it does (a failed round, a cancelled run), with no window in which a task of that job
/// could still be claimed.
pub async fn cancel_job_in(
    tx: &mut gg_storage::PgConnection,
    actor: &str,
    authority: Authority,
    job: JobId,
) -> Result<usize, ApiError> {
    let j = jobs::lock_job(tx, job)
        .await?
        .ok_or_else(|| ApiError::NotFound("job".into()))?;
    if !j.cancelled {
        jobs::set_job_cancelled(tx, job).await?;
    }
    let mut changed = 0;
    for task in jobs::lock_tasks_of_job(tx, job).await? {
        let s = task
            .task_state()
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        if s.is_terminal() {
            continue;
        }
        let next = s.next(TaskEvent::Cancel, authority)?;
        jobs::update_task_state(tx, TaskId(task.id), next).await?;
        changed += 1;
        for a in tasks::list_attempts(tx, TaskId(task.id)).await? {
            let a_state = a
                .attempt_state()
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            if matches!(a_state, AttemptState::Active | AttemptState::Submitted) {
                tasks::update_attempt_state(
                    tx,
                    AttemptId(a.id),
                    a_state.next(AttemptEvent::Supersede)?,
                )
                .await?;
                tasks::release_lease(tx, AttemptId(a.id)).await?;
                gg_ledger::release(tx, AttemptId(a.id)).await?;
            }
        }
    }
    misc::audit(
        tx,
        actor,
        "job.cancel",
        &job.to_string(),
        serde_json::json!({ "tasks_cancelled": changed }),
    )
    .await?;
    misc::outbox_insert(
        tx,
        "job.cancelled",
        &serde_json::json!({ "job_id": job, "tasks_cancelled": changed }),
    )
    .await?;
    Ok(changed)
}
