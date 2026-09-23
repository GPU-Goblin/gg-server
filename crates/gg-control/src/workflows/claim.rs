//! Task assignment: one short transaction reserves a compatible task, its budget and a lease.

use gg_core::*;
use gg_protocol::domain::Domain;
use gg_protocol::messages::{ArtifactRef, AssignmentV1, TaskSpecV1};
use gg_protocol::signed::Signed;
use gg_storage::queries::{identities, jobs, misc, tasks};
use serde::Serialize;

use super::{decode_spec, now_ms};
use crate::error::ApiError;
use crate::state::AppState;

/// A claimed assignment returned to the worker.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct Claimed {
    /// Signed assignment envelope.
    #[schema(value_type = Object)]
    pub assignment: Signed<AssignmentV1>,
    /// Task specification.
    #[schema(value_type = Object)]
    pub spec: TaskSpecV1,
    /// Input artifacts to download.
    #[schema(value_type = Vec<Object>)]
    pub inputs: Vec<NamedArtifact>,
}

/// Named artifact reference.
#[derive(Clone, Debug, Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct NamedArtifact {
    /// Name.
    pub name: String,
    /// Hash.
    pub hash: Sha256Digest,
    /// Size.
    pub size: u64,
}

impl NamedArtifact {
    /// From a map entry.
    pub fn from_ref(name: &str, r: &ArtifactRef) -> Self {
        Self {
            name: name.to_owned(),
            hash: r.hash,
            size: r.size,
        }
    }
}

/// Shortest lease handed out, whatever rhythm a worker announces.
pub const MIN_LEASE_SECS: i64 = 15;

/// Lease length for a worker that heartbeats every `heartbeat_secs`: four missed heartbeats, at
/// least [`MIN_LEASE_SECS`], never more than the configured length. Without an announced rhythm,
/// the configured length.
pub fn lease_for(configured_secs: i64, heartbeat_secs: Option<u64>) -> i64 {
    match heartbeat_secs {
        Some(h) if h > 0 => {
            let four = i64::try_from(h.saturating_mul(4)).unwrap_or(i64::MAX);
            four.max(MIN_LEASE_SECS).min(configured_secs)
        }
        _ => configured_secs,
    }
}

/// Try once to claim a task for `worker` among `kinds`, within what it announced (`caps`), with a
/// lease of `lease_secs`. Returns `None` when nothing is assignable.
pub async fn claim_once(
    state: &AppState,
    worker: WorkerId,
    kinds: &[String],
    caps: &jobs::WorkerCapabilities,
    lease_secs: i64,
) -> Result<Option<Claimed>, ApiError> {
    if kinds.is_empty() {
        return Ok(None);
    }
    let cfg = state.config();
    let mut tx = state.db().begin().await?;
    let mut excluded: Vec<uuid::Uuid> = Vec::new();
    for _ in 0..16 {
        let Some(task) = jobs::claim_candidate(&mut tx, worker, kinds, &excluded, caps).await?
        else {
            break;
        };
        excluded.push(task.id);
        let project = ProjectId(task.project_id);
        let Some(proj) = identities::get_project(&mut tx, project).await? else {
            continue;
        };
        if proj
            .project_state()
            .map_err(|e| ApiError::Internal(e.to_string()))?
            != ProjectState::Active
            || !proj.allowed_kinds.contains(&task.compute_kind)
        {
            continue;
        }
        let spec: TaskSpecV1 = decode_spec(&task.spec_cbor)?;
        let role = AttemptRole::Primary;
        let amount = spec
            .definition
            .reward_schedule
            .reservation_for(role)
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        let budget = gg_ledger::budget(&mut tx, project).await?;
        if budget.available < amount {
            tracing::debug!(task = %task.id, project = %project, "insufficient budget, skipping task");
            continue;
        }
        // Invariant I1: project active, kind allowed, budget reserved below, all in this transaction.
        let task_state = task
            .task_state()
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        task_state.next(TaskEvent::Assign, Authority::Server)?;
        let generation = jobs::assign_task(&mut tx, TaskId(task.id)).await?;
        let attempt_id = AttemptId::new();
        let lease_id = LeaseId::new();
        let now = now_ms();
        let assignment = AssignmentV1 {
            protocol_version: gg_protocol::PROTOCOL_VERSION,
            network: state.network().clone(),
            task_id: TaskId(task.id),
            spec_hash: task
                .spec_digest()
                .map_err(|e| ApiError::Internal(e.to_string()))?,
            attempt_id,
            worker_id: worker,
            lease_id,
            generation: generation as u64,
            role,
            assigned_at: now,
            lease_expires_at: now.plus_secs(lease_secs),
            lease_max_until: now.plus_secs(cfg.lease_max_secs),
        };
        let signed = Signed::sign(
            state.server_key(),
            state.network(),
            Domain::Assignment,
            assignment,
        )?;
        let cbor = signed.to_canonical_bytes()?;
        let hash = signed.body_hash(state.network(), Domain::Assignment)?;
        tasks::insert_attempt(
            &mut tx,
            attempt_id,
            TaskId(task.id),
            worker,
            generation,
            role,
            &cbor,
            &hash,
        )
        .await?;
        tasks::insert_lease(
            &mut tx,
            lease_id,
            attempt_id,
            gg_storage::from_millis(signed.body.lease_expires_at),
            gg_storage::from_millis(signed.body.lease_max_until),
            lease_secs,
        )
        .await?;
        let reservation =
            gg_ledger::reserve(&mut tx, project, TaskId(task.id), attempt_id, amount).await?;
        tasks::set_attempt_reservation(&mut tx, attempt_id, reservation).await?;
        identities::touch_worker(&mut tx, worker).await?;
        misc::outbox_insert(
            &mut tx,
            "task.assigned",
            &serde_json::json!({ "task_id": task.id, "attempt_id": attempt_id, "worker_id": worker, "generation": generation }),
        )
        .await?;
        tx.commit().await?;
        let inputs = spec
            .definition
            .inputs
            .iter()
            .map(|(n, r)| NamedArtifact::from_ref(n, r))
            .collect();
        return Ok(Some(Claimed {
            assignment: signed,
            spec,
            inputs,
        }));
    }
    tx.rollback().await?;
    Ok(None)
}

/// Long-poll claim, up to `wait_secs` (bounded by configuration). It wakes as soon as tasks become
/// claimable (`AppState::signal_work`), and retries every 500 ms regardless. What the worker
/// announced is recorded first, so that `GET /v1/grid/capacity` can answer before any task.
pub async fn claim_wait(
    state: &AppState,
    worker: WorkerId,
    kinds: &[String],
    caps: &jobs::WorkerCapabilities,
    wait_secs: u64,
    lease_secs: i64,
) -> Result<Option<Claimed>, ApiError> {
    {
        let mut c = state.db().pool().acquire().await?;
        identities::announce_capabilities(&mut c, worker, caps).await?;
    }
    let wait = wait_secs.min(state.config().claim_wait_max_secs);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(wait);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let step = left.min(std::time::Duration::from_millis(500));
        if let Some(c) = crate::state::Signals::wait_or(&state.signals().work, step, || {
            claim_once(state, worker, kinds, caps, lease_secs)
        })
        .await?
        {
            return Ok(Some(c));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
    }
}

/// Renew the lease of an active attempt owned by `worker`. Returns the new expiry.
pub async fn heartbeat(
    state: &AppState,
    worker: WorkerId,
    attempt: AttemptId,
) -> Result<UnixMillis, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let a = tasks::get_attempt(&mut c, attempt)
        .await?
        .ok_or_else(|| ApiError::NotFound("attempt".into()))?;
    if a.worker_id != worker.0 {
        return Err(ApiError::NotFound("attempt".into()));
    }
    if a.attempt_state()
        .map_err(|e| ApiError::Internal(e.to_string()))?
        != AttemptState::Active
    {
        return Err(ApiError::Conflict(format!("attempt is {}", a.state)));
    }
    match tasks::renew_lease(&mut c, attempt, state.config().lease_secs).await? {
        Some(t) => Ok(gg_storage::to_millis(t)),
        None => Err(ApiError::Conflict("lease expired".into())),
    }
}

/// Worker gives up an active attempt: the task is re-queued (or fails) and the budget released.
pub async fn abandon(
    state: &AppState,
    worker: WorkerId,
    attempt: AttemptId,
    reason: &str,
) -> Result<TaskState, ApiError> {
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
    if a_state != AttemptState::Active {
        return Err(ApiError::Conflict(format!("attempt is {}", a.state)));
    }
    let task = jobs::lock_task(&mut tx, TaskId(a.task_id))
        .await?
        .ok_or_else(|| ApiError::Internal("task missing".into()))?;
    let next_attempt = a_state.next(AttemptEvent::Abandon)?;
    tasks::update_attempt_state(&mut tx, attempt, next_attempt).await?;
    tasks::release_lease(&mut tx, attempt).await?;
    gg_ledger::release(&mut tx, attempt).await?;
    let t_state = task
        .task_state()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let next_task = if task.generation == a.generation && t_state == TaskState::Assigned {
        let n = t_state.next(
            TaskEvent::LeaseLost {
                attempts_left: super::attempts_left(task.attempts_made, task.max_attempts),
            },
            Authority::Server,
        )?;
        jobs::update_task_state(&mut tx, TaskId(task.id), n).await?;
        n
    } else {
        t_state
    };
    misc::outbox_insert(
        &mut tx,
        "attempt.abandoned",
        &serde_json::json!({ "task_id": task.id, "attempt_id": attempt, "worker_id": worker, "reason": reason.chars().take(200).collect::<String>(), "task_state": next_task }),
    )
    .await?;
    tx.commit().await?;
    Ok(next_task)
}

#[cfg(test)]
mod tests {
    use super::{MIN_LEASE_SECS, lease_for};

    #[test]
    fn a_lease_lasts_four_heartbeats_within_bounds() {
        assert_eq!(lease_for(120, Some(5)), 20);
        assert_eq!(lease_for(120, Some(1)), MIN_LEASE_SECS);
        assert_eq!(
            lease_for(120, Some(60)),
            120,
            "never above the configured length"
        );
        assert_eq!(lease_for(120, None), 120);
        assert_eq!(lease_for(120, Some(0)), 120);
        assert_eq!(
            lease_for(5, Some(5)),
            5,
            "a short configured length stays the bound"
        );
        assert_eq!(lease_for(120, Some(u64::MAX)), 120);
    }
}
