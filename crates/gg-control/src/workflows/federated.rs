//! Federated validation (ADR-0014): unpredictable audits by an administratively distinct operator,
//! combination of the decisions on one submission, and the dispute procedure.
//!
//! Three rules hold everywhere in this module:
//! * a worker never chooses its validators and cannot predict an audit (the selection depends on a
//!   server-side secret and is computed after the result is recorded);
//! * several keys of one operator are one opinion, so quorum counts operators;
//! * a disagreement is a dispute for a human operator, never an automatic sanction.

use gg_core::*;
use gg_storage::queries::federated;
use serde::Serialize;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// Domain string of the audit selection, so the secret cannot be reused elsewhere.
const AUDIT_SELECTION_TAG: &[u8] = b"gpu-goblin/audit-selection/v1\n";
/// Domain string of the engine-check selection (ADR-0031): drawn independently of the audit.
const ENGINE_CHECK_SELECTION_TAG: &[u8] = b"gpu-goblin/engine-check-selection/v1\n";

/// Whether a submission is audited. The secret never leaves the server, and the selection is
/// computed only once the submission is recorded: the worker commits its result before any check is
/// revealed, and cannot influence or predict the draw.
pub fn audit_selected(secret: &[u8], submission_hash: &Sha256Digest, rate: f64) -> bool {
    selected(AUDIT_SELECTION_TAG, secret, submission_hash, rate)
}

/// Whether a submission a GPU validator decided is recomputed again on the CPU (ADR-0031). Same
/// draw as an audit, under another domain: the two selections are independent.
pub fn engine_check_selected(secret: &[u8], submission_hash: &Sha256Digest, rate: f64) -> bool {
    selected(ENGINE_CHECK_SELECTION_TAG, secret, submission_hash, rate)
}

fn selected(tag: &[u8], secret: &[u8], submission_hash: &Sha256Digest, rate: f64) -> bool {
    // NaN or a non-positive rate audits nothing.
    if rate.is_nan() || rate <= 0.0 {
        return false;
    }
    if rate >= 1.0 {
        return true;
    }
    let mut h = Sha256Digest::hasher();
    h.update(tag);
    h.update(secret);
    h.update(submission_hash.as_bytes());
    let digest = h.finalize();
    let draw = u64::from_be_bytes(digest.as_bytes()[..8].try_into().expect("32-byte digest"));
    // Uniform in [0, 1): 2^64 is exact in f64.
    (draw as f64) / 18_446_744_073_709_551_616.0 < rate
}

/// Outcome of combining every decision recorded on one submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Combined {
    /// Some verification is still pending or in progress.
    Awaiting(u32),
    /// Every decision accepted.
    Accepted,
    /// At least one rejection and no acceptance.
    Rejected,
    /// No rejection, at least one inconclusive.
    Inconclusive,
    /// Acceptance and rejection on the same submission: a human decides (no payment, no sanction).
    Disputed,
}

/// Combine decisions. Nothing is paid unless every verification accepted: an audit that could not
/// conclude blocks the payment and sends the task back to the queue.
pub fn combine(outcomes: &[Option<VerificationOutcome>]) -> Combined {
    let pending = outcomes.iter().filter(|o| o.is_none()).count() as u32;
    if pending > 0 {
        return Combined::Awaiting(pending);
    }
    let accepted = outcomes
        .iter()
        .filter(|o| **o == Some(VerificationOutcome::Accepted))
        .count();
    let rejected = outcomes
        .iter()
        .filter(|o| **o == Some(VerificationOutcome::Rejected))
        .count();
    let inconclusive = outcomes
        .iter()
        .filter(|o| **o == Some(VerificationOutcome::Inconclusive))
        .count();
    match (accepted, rejected, inconclusive) {
        (_, r, i) if r > 0 && i == 0 && accepted > 0 => {
            let _ = r;
            Combined::Disputed
        }
        (a, 0, 0) if a > 0 => Combined::Accepted,
        (0, r, _) if r > 0 => Combined::Rejected,
        (_, r, _) if r > 0 => Combined::Disputed,
        _ => Combined::Inconclusive,
    }
}

/// A validator's opinion, as recorded in a dispute.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct Opinion {
    /// Validator that decided.
    pub validator_id: Option<Uuid>,
    /// Its operator (administrative entity).
    pub operator_id: Option<Uuid>,
    /// `primary` or `audit`.
    pub kind: String,
    /// Outcome.
    pub outcome: Option<String>,
    /// Decision id (signed and stored).
    pub decision_id: Option<Uuid>,
}

/// Decide whether to open an audit for a submission, and open it. Returns whether one was created.
/// No audit is created when fewer than two operators can validate: it could never be claimed, since
/// a validator never audits what its own operator decided.
pub async fn maybe_open_audit(
    state: &AppState,
    tx: &mut gg_storage::PgConnection,
    submission: SubmissionId,
    submission_hash: &Sha256Digest,
    task: TaskId,
    attempt: AttemptId,
) -> Result<bool, ApiError> {
    let rate = state.audit_rate();
    if !audit_selected(state.audit_secret(), submission_hash, rate) {
        return Ok(false);
    }
    if federated::active_operator_count(tx).await? < 2 {
        tracing::debug!(
            %submission,
            "submission selected for audit but a single operator can validate: no audit opened"
        );
        return Ok(false);
    }
    let created =
        federated::insert_audit_verification(tx, Uuid::now_v7(), submission, task, attempt).await?;
    if created {
        // Server-side only: nothing in any worker-facing response says a submission is audited.
        tracing::info!(%task, %attempt, "audit opened for this submission");
    }
    Ok(created)
}

/// Opinions recorded on a submission, for disputes and for the researcher's audit trail.
pub async fn opinions(
    tx: &mut gg_storage::PgConnection,
    submission: SubmissionId,
) -> Result<Vec<Opinion>, ApiError> {
    Ok(federated::verifications_of_submission(tx, submission)
        .await?
        .into_iter()
        .map(|v| Opinion {
            validator_id: v.validator_id,
            operator_id: v.operator_id,
            kind: v.kind,
            outcome: v.outcome,
            decision_id: v.decision_id,
        })
        .collect())
}

/// Result of resolving a dispute.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct Resolved {
    /// Dispute.
    pub dispute_id: Uuid,
    /// Outcome an operator settled on.
    pub outcome: VerificationOutcome,
    /// Resulting task state.
    pub task_state: TaskState,
    /// Receipt when the resolution paid the contributor.
    pub receipt_id: Option<ReceiptId>,
}

/// Settle a dispute. An operator may only settle on an outcome a validator actually signed: the
/// resolution chooses between recorded opinions, it never fabricates one. Credits move here, once.
pub async fn resolve(
    state: &AppState,
    actor: &str,
    dispute_id: Uuid,
    outcome: VerificationOutcome,
    reason: &str,
) -> Result<Resolved, ApiError> {
    use gg_storage::queries::{jobs, misc, tasks};

    if reason.len() > 1000 {
        return Err(ApiError::Unprocessable("reason too long".into()));
    }
    let mut tx = state.db().begin().await?;
    let d = federated::lock_dispute(&mut tx, dispute_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("dispute".into()))?;
    if d.state != "open" {
        return Err(ApiError::Conflict("dispute is already resolved".into()));
    }
    let opinions = opinions(&mut tx, SubmissionId(d.submission_id)).await?;
    if !opinions
        .iter()
        .any(|o| o.outcome.as_deref() == Some(outcome.as_str()))
    {
        return Err(ApiError::Unprocessable(format!(
            "no validator signed a '{outcome}' decision on this submission: an operator settles between recorded opinions"
        )));
    }
    let task = jobs::lock_task(&mut tx, TaskId(d.task_id))
        .await?
        .ok_or_else(|| ApiError::Internal("task missing".into()))?;
    let attempt = tasks::lock_attempt(&mut tx, AttemptId(d.attempt_id))
        .await?
        .ok_or_else(|| ApiError::Internal("attempt missing".into()))?;
    let sub = tasks::get_submission(&mut tx, SubmissionId(d.submission_id))
        .await?
        .ok_or_else(|| ApiError::Internal("submission missing".into()))?;
    let t_state = task
        .task_state()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if t_state != TaskState::Disputed {
        return Err(ApiError::Conflict(format!(
            "task is {t_state}, not disputed"
        )));
    }
    // The receipt references the signed decision that supports the settled outcome.
    let supporting = crate::workflows::decide::decision_hash_for(&mut tx, &opinions, outcome)
        .await?
        .ok_or_else(|| ApiError::Internal("no decision to reference".into()))?;
    let all = crate::workflows::decide::all_decision_hashes(&mut tx, &opinions).await?;
    let (task_state, receipt_id) = crate::workflows::decide::finalize(
        state,
        &mut tx,
        &task,
        &attempt,
        &sub,
        outcome,
        supporting,
        &all,
        TaskEvent::Resolve(outcome),
        Authority::Admin,
    )
    .await?;
    federated::resolve_dispute(&mut tx, dispute_id, outcome, actor, reason).await?;
    misc::audit(
        &mut tx,
        actor,
        "dispute.resolve",
        &dispute_id.to_string(),
        serde_json::json!({ "outcome": outcome, "task_id": d.task_id, "reason": reason }),
    )
    .await?;
    misc::outbox_insert(
        &mut tx,
        "dispute.resolved",
        &serde_json::json!({ "dispute_id": dispute_id, "task_id": d.task_id, "outcome": outcome }),
    )
    .await?;
    tx.commit().await?;
    tracing::info!(%dispute_id, %outcome, task_id = %d.task_id, "dispute resolved by an operator");
    Ok(Resolved {
        dispute_id,
        outcome,
        task_state,
        receipt_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use VerificationOutcome::*;

    #[test]
    fn nothing_is_paid_unless_every_verification_accepted() {
        assert_eq!(combine(&[Some(Accepted)]), Combined::Accepted);
        assert_eq!(
            combine(&[Some(Accepted), Some(Accepted)]),
            Combined::Accepted
        );
        assert_eq!(combine(&[Some(Accepted), None]), Combined::Awaiting(1));
        assert_eq!(combine(&[None, None]), Combined::Awaiting(2));
        // Disagreement: no payment, no sanction, a human decides.
        assert_eq!(
            combine(&[Some(Accepted), Some(Rejected)]),
            Combined::Disputed
        );
        assert_eq!(
            combine(&[Some(Rejected), Some(Accepted)]),
            Combined::Disputed
        );
        // A single rejection with no acceptance is simply a rejection.
        assert_eq!(
            combine(&[Some(Rejected), Some(Rejected)]),
            Combined::Rejected
        );
        assert_eq!(combine(&[Some(Rejected)]), Combined::Rejected);
        // An audit that cannot conclude blocks the payment rather than trusting the other opinion.
        assert_eq!(
            combine(&[Some(Accepted), Some(Inconclusive)]),
            Combined::Inconclusive
        );
        assert_eq!(
            combine(&[Some(Inconclusive), Some(Inconclusive)]),
            Combined::Inconclusive
        );
        // Rejection plus inconclusive is a rejection, not a dispute: nobody vouched for the result.
        assert_eq!(
            combine(&[Some(Rejected), Some(Inconclusive)]),
            Combined::Rejected
        );
    }

    #[test]
    fn audit_selection_is_deterministic_secret_dependent_and_matches_its_rate() {
        let hash = Sha256Digest::of_bytes(b"a submission");
        assert!(!audit_selected(b"secret", &hash, 0.0));
        assert!(audit_selected(b"secret", &hash, 1.0));
        // Same secret and submission: same answer; another secret: independent draw.
        let a = audit_selected(b"secret-a", &hash, 0.5);
        assert_eq!(a, audit_selected(b"secret-a", &hash, 0.5));
        let differing = (0..64)
            .filter(|i| {
                let h = Sha256Digest::of_bytes(format!("submission {i}").as_bytes());
                audit_selected(b"secret-a", &h, 0.5) != audit_selected(b"secret-b", &h, 0.5)
            })
            .count();
        assert!(
            differing > 10,
            "the draw must depend on the secret ({differing}/64 differed)"
        );
        // The rate is honoured on a large sample.
        let audited = (0..2000)
            .filter(|i| {
                let h = Sha256Digest::of_bytes(format!("s{i}").as_bytes());
                audit_selected(b"secret", &h, 0.25)
            })
            .count();
        assert!(
            (400..=600).contains(&audited),
            "expected about 500 of 2000 audited, got {audited}"
        );
    }
}
