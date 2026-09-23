//! Result intake: signature, lease, generation and artifact checks; one canonical result per generation.

use gg_core::*;
use gg_protocol::domain::Domain;
use gg_protocol::keys::PublicKey;
use gg_protocol::messages::ResultSubmissionV1;
use gg_protocol::signed::Signed;
use gg_storage::queries::{artifacts, jobs, misc, tasks};
use serde::Serialize;

use crate::error::ApiError;
use crate::state::AppState;

/// Outcome of a submission.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct Submitted {
    /// Submission id.
    pub submission_id: SubmissionId,
    /// Domain-separated hash of the submission body.
    pub submission_hash: Sha256Digest,
    /// True when this call recorded the submission; false when it was an identical replay.
    pub recorded: bool,
}

/// Validate and record a signed result for `attempt`.
pub async fn submit_result(
    state: &AppState,
    worker: WorkerId,
    worker_key: PublicKey,
    attempt: AttemptId,
    signed: Signed<ResultSubmissionV1>,
) -> Result<Submitted, ApiError> {
    if signed.signer != worker_key {
        return Err(ApiError::Forbidden(
            "submission must be signed by the session key".into(),
        ));
    }
    let body = &signed.body;
    body.validate_structure()?;
    if body.network != *state.network() {
        return Err(ApiError::Unprocessable("network mismatch".into()));
    }
    if body.attempt_id != attempt {
        return Err(ApiError::Unprocessable("attempt id mismatch".into()));
    }
    let hash = signed.verify(state.network(), Domain::Result)?;
    let cbor = signed.to_canonical_bytes()?;

    let mut tx = state.db().begin().await?;
    let a = tasks::lock_attempt(&mut tx, attempt)
        .await?
        .ok_or_else(|| ApiError::NotFound("attempt".into()))?;
    if a.worker_id != worker.0 {
        return Err(ApiError::NotFound("attempt".into()));
    }
    let a_state = a
        .attempt_state()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if a_state == AttemptState::Submitted {
        // Replay: identical content → same answer; different content → conflict (I4).
        let existing = tasks::get_submission_by_attempt(&mut tx, attempt).await?;
        return match existing {
            Some(s) if s.digest().map_err(|e| ApiError::Internal(e.to_string()))? == hash => {
                Ok(Submitted {
                    submission_id: SubmissionId(s.id),
                    submission_hash: hash,
                    recorded: false,
                })
            }
            _ => Err(ApiError::Conflict(
                "a different result was already submitted for this attempt".into(),
            )),
        };
    }
    if !a_state.accepts_submission() {
        return Err(ApiError::Conflict(format!("attempt is {}", a.state)));
    }
    if !tasks::lease_is_valid(&mut tx, attempt).await? {
        // Invariant I3: a late message never revives an expired lease.
        return Err(ApiError::Conflict("lease expired".into()));
    }
    let task = jobs::lock_task(&mut tx, TaskId(a.task_id))
        .await?
        .ok_or_else(|| ApiError::Internal("task missing".into()))?;
    let t_state = task
        .task_state()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if t_state != TaskState::Assigned
        || task.generation != a.generation
        || body.generation != a.generation as u64
    {
        return Err(ApiError::Conflict(
            "task generation no longer matches this attempt".into(),
        ));
    }
    if body.task_id != TaskId(task.id)
        || body.spec_hash
            != task
                .spec_digest()
                .map_err(|e| ApiError::Internal(e.to_string()))?
        || body.assignment_hash
            != a.assignment_digest()
                .map_err(|e| ApiError::Internal(e.to_string()))?
    {
        return Err(ApiError::Unprocessable(
            "submission does not reference this assignment".into(),
        ));
    }
    // An activation of a stage task may only be declared: its durable copy will follow, and
    // nothing is verified before it arrives (ADR-0026, bounded exception to invariant I7 of docs/SPEC.md).
    let staged = matches!(
        super::decode_spec(&task.spec_cbor)?.definition.params,
        gg_protocol::messages::TaskParamsV1::GraphStage(_)
    );
    for (name, r) in &body.outputs {
        let row = artifacts::get_artifact(&mut tx, &r.hash).await?;
        if row.is_none() && staged && (name.starts_with("out.") || name.starts_with("din.")) {
            let declared =
                gg_storage::queries::pending::get_for_attempt(&mut tx, &r.hash, attempt.0).await?;
            match declared {
                Some(p) if p.size == r.size && p.project_id == task.project_id => continue,
                _ => {
                    return Err(ApiError::Unprocessable(format!(
                        "output '{name}' was neither uploaded nor declared by this attempt"
                    )));
                }
            }
        }
        let row = row
            .ok_or_else(|| ApiError::Unprocessable(format!("output '{name}' was not uploaded")))?;
        if row.size as u64 != r.size
            || row.project_id != Some(task.project_id)
            || !state.objects().exists(&r.hash)
        {
            return Err(ApiError::Unprocessable(format!(
                "output '{name}' does not match a stored artifact of this project"
            )));
        }
    }
    let submission_id = SubmissionId::new();
    if let Err(e) = tasks::insert_submission(
        &mut tx,
        submission_id,
        TaskId(task.id),
        attempt,
        a.generation,
        &hash,
        &cbor,
    )
    .await
    {
        if e.is_unique_violation() {
            return Err(ApiError::Conflict(
                "a result already exists for this task generation".into(),
            ));
        }
        return Err(e.into());
    }
    tasks::update_attempt_state(&mut tx, attempt, a_state.next(AttemptEvent::Submit)?).await?;
    jobs::update_task_state(
        &mut tx,
        TaskId(task.id),
        t_state.next(TaskEvent::Submit, Authority::Worker)?,
    )
    .await?;
    tasks::release_lease(&mut tx, attempt).await?;
    tasks::insert_verification(
        &mut tx,
        uuid::Uuid::now_v7(),
        submission_id,
        TaskId(task.id),
        attempt,
    )
    .await?;
    // Unpredictable audit (ADR-0014): drawn from a server-side secret *after* the result is
    // recorded. The worker has already committed; nothing in its answer says whether it was drawn.
    super::federated::maybe_open_audit(
        state,
        &mut tx,
        submission_id,
        &hash,
        TaskId(task.id),
        attempt,
    )
    .await?;
    misc::outbox_insert(
        &mut tx,
        "result.submitted",
        &serde_json::json!({ "task_id": task.id, "attempt_id": attempt, "submission_id": submission_id, "profile": body.execution_profile.numeric_profile }),
    )
    .await?;
    tx.commit().await?;
    state.signal_submission();
    Ok(Submitted {
        submission_id,
        submission_hash: hash,
        recorded: true,
    })
}
