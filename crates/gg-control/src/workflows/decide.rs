//! Verification claim and decision application: transitions, budget commit/release, single reward,
//! signed receipt.

use gg_core::*;
use gg_protocol::domain::Domain;
use gg_protocol::keys::PublicKey;
use gg_protocol::messages::{
    ContributionReceiptV1, ResultSubmissionV1, TaskSpecV1, VerificationDecisionV1,
};
use gg_protocol::signed::Signed;
use gg_storage::queries::{federated, identities, jobs, misc, tasks};
use serde::Serialize;

use super::{decode_spec, now_ms};
use crate::error::ApiError;
use crate::state::AppState;
use crate::workflows::claim::NamedArtifact;

/// Verification work handed to a validator.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct VerificationWork {
    /// Verification id.
    pub verification_id: uuid::Uuid,
    /// Task id.
    pub task_id: TaskId,
    /// Attempt id.
    pub attempt_id: AttemptId,
    /// Task specification.
    #[schema(value_type = Object)]
    pub spec: TaskSpecV1,
    /// Signed submission.
    #[schema(value_type = Object)]
    pub submission: Signed<ResultSubmissionV1>,
    /// Inputs to download.
    pub inputs: Vec<NamedArtifact>,
    /// Outputs to download.
    pub outputs: Vec<NamedArtifact>,
    /// Lease expiry.
    pub lease_expires_at: UnixMillis,
}

/// Claim one verification for a validator. `cpu_reference`: the validator recomputes on the CPU
/// reference profile, and may therefore take engine checks (ADR-0031).
pub async fn claim_verification_once(
    state: &AppState,
    validator: ValidatorId,
    cpu_reference: bool,
    secondary_only: bool,
) -> Result<Option<VerificationWork>, ApiError> {
    let mut tx = state.db().begin().await?;
    // The oldest verification whose inputs and outputs all have their durable copy: an activation
    // that travelled before it (ADR-0026) is verified only once the server holds its bytes.
    let mut picked = None;
    for (id, task_id, submission_id) in
        tasks::lock_verification_candidates(&mut tx, validator, 16, cpu_reference, secondary_only)
            .await?
    {
        let task = jobs::get_task(&mut tx, TaskId(task_id))
            .await?
            .ok_or_else(|| ApiError::Internal("task missing".into()))?;
        let sub = tasks::get_submission(&mut tx, SubmissionId(submission_id))
            .await?
            .ok_or_else(|| ApiError::Internal("submission missing".into()))?;
        let spec = decode_spec(&task.spec_cbor)?;
        let submission: Signed<ResultSubmissionV1> =
            Signed::from_canonical_bytes(&sub.submission_cbor)
                .map_err(|e| ApiError::Internal(format!("stored submission undecodable: {e}")))?;
        let hashes: Vec<Sha256Digest> = spec
            .definition
            .inputs
            .values()
            .chain(submission.body.outputs.values())
            .map(|r| r.hash)
            .collect();
        if !gg_storage::queries::pending::still_pending(&mut tx, &hashes)
            .await?
            .is_empty()
        {
            continue;
        }
        picked = Some((id, task, sub));
        break;
    }
    let Some((id, task, sub)) = picked else {
        tx.rollback().await?;
        return Ok(None);
    };
    let v = tasks::start_verification(
        &mut tx,
        id,
        validator,
        state.config().verification_lease_secs,
    )
    .await?
    .ok_or_else(|| ApiError::Internal("locked verification vanished".into()))?;
    tx.commit().await?;
    let spec = decode_spec(&task.spec_cbor)?;
    let submission: Signed<ResultSubmissionV1> = Signed::from_canonical_bytes(&sub.submission_cbor)
        .map_err(|e| ApiError::Internal(format!("stored submission undecodable: {e}")))?;
    let inputs = spec
        .definition
        .inputs
        .iter()
        .map(|(n, r)| NamedArtifact::from_ref(n, r))
        .collect();
    let outputs = submission
        .body
        .outputs
        .iter()
        .map(|(n, r)| NamedArtifact::from_ref(n, r))
        .collect();
    Ok(Some(VerificationWork {
        verification_id: v.id,
        task_id: TaskId(v.task_id),
        attempt_id: AttemptId(v.attempt_id),
        spec,
        submission,
        inputs,
        outputs,
        lease_expires_at: v
            .lease_expires_at
            .map(gg_storage::to_millis)
            .unwrap_or_default(),
    }))
}

/// Long-poll variant.
pub async fn claim_verification_wait(
    state: &AppState,
    validator: ValidatorId,
    wait_secs: u64,
    cpu_reference: bool,
    secondary_only: bool,
) -> Result<Option<VerificationWork>, ApiError> {
    let wait = wait_secs.min(state.config().claim_wait_max_secs);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(wait);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let step = left.min(std::time::Duration::from_millis(500));
        if let Some(w) = crate::state::Signals::wait_or(&state.signals().verification, step, || {
            claim_verification_once(state, validator, cpu_reference, secondary_only)
        })
        .await?
        {
            return Ok(Some(w));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
    }
}

/// Result of applying a decision.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct DecisionApplied {
    /// Decision id.
    pub decision_id: DecisionId,
    /// Hash of the decision body.
    pub decision_hash: Sha256Digest,
    /// Outcome.
    pub outcome: VerificationOutcome,
    /// Whether the decision changed the task state (false for replays or superseded attempts, and
    /// while another verification of the same submission is still pending).
    pub applied: bool,
    /// Resulting task state.
    pub task_state: TaskState,
    /// Receipt id when a reward was issued.
    pub receipt_id: Option<ReceiptId>,
    /// Verifications of this submission still to be decided (ADR-0014). A validator sees that its
    /// decision is not the last word; it never learns who else is checking.
    pub awaiting: u32,
    /// Dispute opened because decisions disagreed.
    pub dispute_id: Option<uuid::Uuid>,
    /// Whether this call recorded the decision (false for an identical replay). Not sent.
    #[serde(skip)]
    #[schema(ignore)]
    pub newly_recorded: bool,
}

/// Apply a signed decision from the validator holding the verification.
pub async fn apply_decision(
    state: &AppState,
    validator: ValidatorId,
    validator_key: PublicKey,
    verification_id: uuid::Uuid,
    signed: Signed<VerificationDecisionV1>,
) -> Result<DecisionApplied, ApiError> {
    if signed.signer != validator_key {
        return Err(ApiError::Forbidden(
            "decision must be signed by the session key".into(),
        ));
    }
    let body = &signed.body;
    body.validate_structure()?;
    if body.network != *state.network() || body.validator_id != validator {
        return Err(ApiError::Unprocessable(
            "network or validator mismatch".into(),
        ));
    }
    let hash = signed.verify(state.network(), Domain::Verification)?;
    let cbor = signed.to_canonical_bytes()?;

    let mut tx = state.db().begin().await?;
    let v = tasks::lock_verification(&mut tx, verification_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("verification".into()))?;
    if body.attempt_id != AttemptId(v.attempt_id) || body.task_id != TaskId(v.task_id) {
        return Err(ApiError::Unprocessable(
            "decision does not reference this verification".into(),
        ));
    }
    let task = jobs::lock_task(&mut tx, TaskId(v.task_id))
        .await?
        .ok_or_else(|| ApiError::Internal("task missing".into()))?;
    if v.verification_state()
        .map_err(|e| ApiError::Internal(e.to_string()))?
        == VerificationState::Decided
    {
        let existing = tasks::get_decision_by_verification(&mut tx, verification_id).await?;
        return match existing {
            Some(d) if d.digest().map_err(|e| ApiError::Internal(e.to_string()))? == hash => {
                let receipt =
                    tasks::get_receipt(&mut tx, AttemptId(v.attempt_id), AttemptRole::Primary)
                        .await?;
                Ok(DecisionApplied {
                    decision_id: DecisionId(d.id),
                    decision_hash: hash,
                    outcome: body.outcome,
                    applied: false,
                    task_state: task
                        .task_state()
                        .map_err(|e| ApiError::Internal(e.to_string()))?,
                    receipt_id: receipt.map(|r| ReceiptId(r.id)),
                    awaiting: 0,
                    dispute_id: None,
                    newly_recorded: false,
                })
            }
            _ => Err(ApiError::Conflict(
                "a different decision was already recorded".into(),
            )),
        };
    }
    if v.validator_id != Some(validator.0) {
        return Err(ApiError::Forbidden(
            "verification is held by another validator".into(),
        ));
    }
    let sub = tasks::get_submission(&mut tx, SubmissionId(v.submission_id))
        .await?
        .ok_or_else(|| ApiError::Internal("submission missing".into()))?;
    if sub
        .digest()
        .map_err(|e| ApiError::Internal(e.to_string()))?
        != body.submission_hash
    {
        return Err(ApiError::Unprocessable(
            "decision references another submission".into(),
        ));
    }
    // The reference must be admitted for the kind (ADR-0031): the CPU everywhere, the GPU for model
    // tasks; an engine check exists to be computed on the CPU.
    let kind = decode_spec(&task.spec_cbor)?
        .definition
        .params
        .compute_kind()
        .to_owned();
    let verification_kind = federated::verification_kind(&mut tx, verification_id).await?;
    let cpu = body.reference_profile == gg_compute::profiles::REFERENCE_PROFILE;
    if !gg_compute::profiles::reference_admitted_for(&kind, &body.reference_profile)
        || (verification_kind == "engine_check" && !cpu)
    {
        return Err(ApiError::Unprocessable(format!(
            "reference profile '{}' is not admitted for a {verification_kind} verification of {kind}",
            body.reference_profile
        )));
    }
    let decision_id = body.decision_id;
    tasks::insert_decision(
        &mut tx,
        decision_id,
        verification_id,
        validator,
        &hash,
        &cbor,
        body.outcome,
    )
    .await?;
    tasks::set_verification_decided(&mut tx, verification_id).await?;
    // A reference computed on the GPU shares the GPU workers' software: a sample goes to a CPU
    // validator as well, before anything is aggregated or paid (ADR-0031).
    if !cpu
        && verification_kind != "engine_check"
        && super::federated::engine_check_selected(
            state.audit_secret(),
            &body.submission_hash,
            state.config().engine_check_rate,
        )
        && federated::insert_engine_check_verification(
            &mut tx,
            uuid::Uuid::now_v7(),
            SubmissionId(v.submission_id),
            TaskId(v.task_id),
            AttemptId(v.attempt_id),
        )
        .await?
    {
        tracing::info!(task_id = %v.task_id, "engine check opened for a GPU-verified submission");
    }

    let a = tasks::lock_attempt(&mut tx, AttemptId(v.attempt_id))
        .await?
        .ok_or_else(|| ApiError::Internal("attempt missing".into()))?;
    let a_state = a
        .attempt_state()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let t_state = task
        .task_state()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if a_state != AttemptState::Submitted
        || t_state != TaskState::Submitted
        || task.generation != a.generation
    {
        // Cancelled or superseded meanwhile: the decision is recorded for audit but opens no right (I8).
        misc::outbox_insert(
            &mut tx,
            "decision.recorded_not_applied",
            &serde_json::json!({ "task_id": task.id, "attempt_id": a.id, "outcome": body.outcome }),
        )
        .await?;
        tx.commit().await?;
        state.signal_decision();
        return Ok(DecisionApplied {
            decision_id,
            decision_hash: hash,
            outcome: body.outcome,
            applied: false,
            task_state: t_state,
            receipt_id: None,
            awaiting: 0,
            dispute_id: None,
            newly_recorded: true,
        });
    }
    // Combine every verification of this submission (ADR-0014). Nothing is paid while an audit is
    // pending, and nothing is paid unless every verification accepted.
    let opinions = super::federated::opinions(&mut tx, SubmissionId(v.submission_id)).await?;
    let outcomes: Vec<Option<VerificationOutcome>> = opinions
        .iter()
        .map(|o| {
            o.outcome
                .as_deref()
                .and_then(|s| s.parse::<VerificationOutcome>().ok())
        })
        .collect();
    let combined = super::federated::combine(&outcomes);
    if let super::federated::Combined::Awaiting(n) = combined {
        misc::outbox_insert(
            &mut tx,
            "decision.recorded_awaiting_audit",
            &serde_json::json!({ "task_id": task.id, "attempt_id": a.id, "outcome": body.outcome, "awaiting": n }),
        )
        .await?;
        tx.commit().await?;
        state.signal_decision();
        tracing::info!(task_id = %task.id, awaiting = n, "decision recorded, waiting for the other verification");
        return Ok(DecisionApplied {
            decision_id,
            decision_hash: hash,
            outcome: body.outcome,
            applied: false,
            task_state: t_state,
            receipt_id: None,
            awaiting: n,
            dispute_id: None,
            newly_recorded: true,
        });
    }
    if combined == super::federated::Combined::Disputed {
        let dispute_id = uuid::Uuid::now_v7();
        let created = gg_storage::queries::federated::insert_dispute(
            &mut tx,
            dispute_id,
            SubmissionId(v.submission_id),
            TaskId(task.id),
            AttemptId(a.id),
            &serde_json::to_value(&opinions).map_err(|e| ApiError::Internal(e.to_string()))?,
        )
        .await?;
        // The attempt stays submitted and the budget stays reserved: a divergence is not a sanction,
        // and nothing moves until an operator decides.
        let next_task = t_state.next(TaskEvent::Dispute, Authority::Server)?;
        jobs::update_task_state(&mut tx, TaskId(task.id), next_task).await?;
        misc::outbox_insert(
            &mut tx,
            "verification.disputed",
            &serde_json::json!({ "task_id": task.id, "attempt_id": a.id, "dispute_id": dispute_id, "opinions": opinions }),
        )
        .await?;
        tx.commit().await?;
        state.signal_decision();
        tracing::warn!(task_id = %task.id, %dispute_id, "validators disagreed: dispute opened, nothing paid");
        return Ok(DecisionApplied {
            decision_id,
            decision_hash: hash,
            outcome: body.outcome,
            applied: true,
            task_state: next_task,
            receipt_id: None,
            awaiting: 0,
            dispute_id: created.then_some(dispute_id),
            newly_recorded: true,
        });
    }
    let final_outcome = match combined {
        super::federated::Combined::Accepted => VerificationOutcome::Accepted,
        super::federated::Combined::Rejected => VerificationOutcome::Rejected,
        _ => VerificationOutcome::Inconclusive,
    };
    let decision_hashes = all_decision_hashes(&mut tx, &opinions).await?;
    let (next_task, receipt_id) = finalize(
        state,
        &mut tx,
        &task,
        &a,
        &sub,
        final_outcome,
        hash,
        &decision_hashes,
        TaskEvent::Decide(final_outcome),
        Authority::Validator,
    )
    .await?;
    tx.commit().await?;
    state.signal_decision();
    Ok(DecisionApplied {
        decision_id,
        decision_hash: hash,
        outcome: final_outcome,
        applied: true,
        task_state: next_task,
        receipt_id,
        awaiting: 0,
        dispute_id: None,
        newly_recorded: true,
    })
}

/// Hash of the signed decision that supports `outcome`, if a validator signed one.
pub(crate) async fn decision_hash_for(
    tx: &mut gg_storage::PgConnection,
    opinions: &[super::federated::Opinion],
    outcome: VerificationOutcome,
) -> Result<Option<Sha256Digest>, ApiError> {
    for o in opinions {
        if o.outcome.as_deref() != Some(outcome.as_str()) {
            continue;
        }
        let Some(id) = o.decision_id else { continue };
        if let Some(d) = tasks::get_decision(tx, id).await? {
            return Ok(Some(
                d.digest().map_err(|e| ApiError::Internal(e.to_string()))?,
            ));
        }
    }
    Ok(None)
}

/// Hashes of every signed decision recorded on a submission, in verification order.
pub(crate) async fn all_decision_hashes(
    tx: &mut gg_storage::PgConnection,
    opinions: &[super::federated::Opinion],
) -> Result<Vec<Sha256Digest>, ApiError> {
    let mut out = Vec::new();
    for o in opinions {
        let Some(id) = o.decision_id else { continue };
        if let Some(d) = tasks::get_decision(tx, id).await? {
            out.push(d.digest().map_err(|e| ApiError::Internal(e.to_string()))?);
        }
    }
    Ok(out)
}

/// Apply a final outcome to an attempt: credits, receipt and task state. Called once per attempt,
/// either when the verifications agree or when an operator resolves a dispute.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn finalize(
    state: &AppState,
    tx: &mut gg_storage::PgConnection,
    task: &gg_storage::models::TaskRow,
    a: &gg_storage::models::AttemptRow,
    sub: &gg_storage::models::SubmissionRow,
    outcome: VerificationOutcome,
    decision_hash: Sha256Digest,
    decision_hashes: &[Sha256Digest],
    event: TaskEvent,
    authority: Authority,
) -> Result<(TaskState, Option<ReceiptId>), ApiError> {
    let a_state = a
        .attempt_state()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let t_state = task
        .task_state()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    tasks::update_attempt_state(
        tx,
        AttemptId(a.id),
        a_state.next(AttemptEvent::Decide(outcome))?,
    )
    .await?;
    let mut next_task = t_state.next(event, authority)?;
    let mut receipt_id = None;
    match outcome {
        VerificationOutcome::Accepted => {
            let spec = decode_spec(&task.spec_cbor)?;
            // A replica's assignment says nothing of its role (ADR-0042): it is its task's.
            let role = if task.replica_of.is_some() {
                AttemptRole::Replica
            } else {
                a.attempt_role()
                    .map_err(|e| ApiError::Internal(e.to_string()))?
            };
            let reward = spec.definition.reward_schedule.for_role(role);
            // A replicated computation (ADR-0042) is verified by the other computations, which are
            // paid their own reward: no validator's recomputation to charge.
            let vcost = if super::replicas::replicated(tx, task).await? {
                Credits::ZERO
            } else {
                spec.definition.reward_schedule.verification_cost
            };
            let key = gg_ledger::reward_key(AttemptId(a.id), role);
            let posting = gg_ledger::commit_and_reward(
                tx,
                AttemptId(a.id),
                WorkerId(a.worker_id),
                reward,
                vcost,
                &key,
            )
            .await?;
            let worker = identities::get_worker(tx, WorkerId(a.worker_id))
                .await?
                .ok_or_else(|| ApiError::Internal("worker missing".into()))?;
            let submission: Signed<ResultSubmissionV1> =
                Signed::from_canonical_bytes(&sub.submission_cbor)
                    .map_err(|e| ApiError::Internal(e.to_string()))?;
            let rid = ReceiptId::new();
            let receipt = ContributionReceiptV1 {
                protocol_version: gg_protocol::PROTOCOL_VERSION,
                network: state.network().clone(),
                receipt_id: rid,
                project_id: spec.project_id,
                experiment_id: spec.experiment_id,
                job_id: spec.job_id,
                task_id: spec.task_id,
                attempt_id: AttemptId(a.id),
                role,
                spec_hash: task
                    .spec_digest()
                    .map_err(|e| ApiError::Internal(e.to_string()))?,
                definition_hash: task
                    .definition_digest()
                    .map_err(|e| ApiError::Internal(e.to_string()))?,
                input_hashes: spec.definition.inputs.values().map(|r| r.hash).collect(),
                submission_hash: sub
                    .digest()
                    .map_err(|e| ApiError::Internal(e.to_string()))?,
                output_hashes: submission.body.outputs.values().map(|r| r.hash).collect(),
                decision_hash,
                decision_hashes: decision_hashes.to_vec(),
                worker_id: WorkerId(a.worker_id),
                worker_public_key: PublicKey(
                    worker
                        .public_key
                        .as_slice()
                        .try_into()
                        .map_err(|_| ApiError::Internal("bad worker key".into()))?,
                ),
                beneficiary: worker.payout_address.clone(),
                amount: reward,
                asset: gg_core::credits::CREDIT_UNIT.to_owned(),
                issued_at: now_ms(),
            };
            let signed_receipt = Signed::sign(
                state.server_key(),
                state.network(),
                Domain::Receipt,
                receipt,
            )?;
            let rhash = signed_receipt.body_hash(state.network(), Domain::Receipt)?;
            let rcbor = signed_receipt.to_canonical_bytes()?;
            tasks::insert_receipt(
                tx,
                rid,
                AttemptId(a.id),
                WorkerId(a.worker_id),
                spec.project_id,
                role,
                reward,
                Some(posting.txn),
                &rhash,
                &rcbor,
            )
            .await?;
            receipt_id = Some(rid);
            misc::outbox_insert(tx, "task.accepted", &serde_json::json!({ "task_id": task.id, "attempt_id": a.id, "receipt_id": rid, "amount": reward, "decisions": decision_hashes.len() })).await?;
        }
        VerificationOutcome::Rejected | VerificationOutcome::Inconclusive => {
            gg_ledger::release(tx, AttemptId(a.id)).await?;
            let left = super::attempts_left(task.attempts_made, task.max_attempts);
            next_task = next_task.next(
                TaskEvent::Retry {
                    attempts_left: left,
                },
                Authority::Server,
            )?;
            misc::outbox_insert(tx, "task.not_accepted", &serde_json::json!({ "task_id": task.id, "attempt_id": a.id, "outcome": outcome, "next_state": next_task })).await?;
        }
    }
    jobs::update_task_state(tx, TaskId(task.id), next_task).await?;
    Ok((next_task, receipt_id))
}

/// Release a verification held by `validator` (it becomes claimable again immediately).
pub async fn release_verification(
    state: &AppState,
    validator: ValidatorId,
    id: uuid::Uuid,
) -> Result<bool, ApiError> {
    let mut tx = state.db().begin().await?;
    let released = tasks::release_verification(&mut tx, id, validator).await?;
    if released {
        misc::outbox_insert(
            &mut tx,
            "verification.released",
            &serde_json::json!({ "verification_id": id, "validator_id": validator }),
        )
        .await?;
    }
    tx.commit().await?;
    state.signals().verification.notify_waiters();
    Ok(released)
}
