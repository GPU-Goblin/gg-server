//! Withdrawing an artifact (INCIDENTS.md, "harmful or mistaken artifact"): its bytes are deleted,
//! every job and run that still needs it is cancelled with its budget released, no worker keeps
//! advertising it, and its hash is never served or accepted again.

use gg_core::*;
use gg_storage::queries::{artifacts, misc};
use serde::Serialize;

use crate::error::ApiError;
use crate::state::AppState;

/// What a withdrawal did.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct Withdrawn {
    /// The artifact.
    pub hash: Sha256Digest,
    /// Collective runs cancelled because they used it.
    pub runs_cancelled: usize,
    /// Jobs cancelled because one of their tasks still needed it.
    pub jobs_cancelled: usize,
    /// Tasks moved to `cancelled`.
    pub tasks_cancelled: usize,
    /// Whether bytes were deleted now (false when a previous withdrawal already had).
    pub bytes_deleted: bool,
}

/// Withdraw `hash`. Idempotent: withdrawing again finishes a withdrawal interrupted before its
/// bytes were deleted, and cancels anything that started using the hash in between.
pub async fn withdraw_artifact(
    state: &AppState,
    actor: &str,
    hash: Sha256Digest,
    reason: &str,
) -> Result<Withdrawn, ApiError> {
    let reason = reason.trim();
    if reason.is_empty() || reason.len() > 500 {
        return Err(ApiError::BadRequest(
            "a reason of 1 to 500 characters is required".into(),
        ));
    }
    let mut tx = state.db().begin().await?;
    if artifacts::get_artifact(&mut tx, &hash).await?.is_none() {
        return Err(ApiError::NotFound("artifact".into()));
    }
    artifacts::mark_withdrawn(&mut tx, &hash, reason).await?;
    let failure = format!("artifact {hash} withdrawn: {reason}");
    let runs = artifacts::runs_referencing(&mut tx, &hash).await?;
    for run in &runs {
        crate::workflows::collective::cancel_run_in(&mut tx, actor, *run, &failure).await?;
    }
    // Jobs still referencing it once the runs' own jobs are cancelled.
    let jobs = artifacts::jobs_referencing(&mut tx, &hash).await?;
    let mut tasks_cancelled = 0;
    for job in &jobs {
        tasks_cancelled +=
            crate::workflows::cancel::cancel_job_in(&mut tx, actor, Authority::Admin, JobId(*job))
                .await?;
    }
    artifacts::delete_peers_of(&mut tx, &hash).await?;
    misc::audit(
        &mut tx,
        actor,
        "artifact.withdraw",
        &hash.to_string(),
        serde_json::json!({
            "reason": reason,
            "runs_cancelled": runs.len(),
            "jobs_cancelled": jobs.len(),
            "tasks_cancelled": tasks_cancelled,
        }),
    )
    .await?;
    misc::outbox_insert(
        &mut tx,
        "artifact.withdrawn",
        &serde_json::json!({ "hash": hash, "reason": reason }),
    )
    .await?;
    tx.commit().await?;
    for run in &runs {
        state.step_cache().remove_run(*run);
    }
    // The bytes go only once nothing refers to them any more; downloads already refuse the hash.
    let bytes_deleted = state.objects().remove(&hash).await?;
    state.signal_submission();
    Ok(Withdrawn {
        hash,
        runs_cancelled: runs.len(),
        jobs_cancelled: jobs.len(),
        tasks_cancelled,
        bytes_deleted,
    })
}
