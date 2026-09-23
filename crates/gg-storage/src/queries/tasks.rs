//! Attempts, leases, reservations, submissions, verifications, decisions and receipts.

use ::time::OffsetDateTime;
use gg_core::*;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::StorageError;
use crate::models::*;

/// Insert an active attempt.
#[allow(clippy::too_many_arguments)]
pub async fn insert_attempt(
    c: &mut PgConnection,
    id: AttemptId,
    task: TaskId,
    worker: WorkerId,
    generation: i64,
    role: AttemptRole,
    assignment_cbor: &[u8],
    assignment_hash: &Sha256Digest,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO attempts (id, task_id, worker_id, generation, role, state, assignment_cbor, assignment_hash)
         VALUES ($1, $2, $3, $4, $5, 'active', $6, $7)",
    )
    .bind(id.0)
    .bind(task.0)
    .bind(worker.0)
    .bind(generation)
    .bind(role.as_str())
    .bind(assignment_cbor)
    .bind(assignment_hash.as_bytes().as_slice())
    .execute(c)
    .await?;
    Ok(())
}

const ATTEMPT_COLS: &str = "id, task_id, worker_id, generation, role, state, assignment_cbor, assignment_hash, reservation_id, created_at";

/// Get an attempt.
pub async fn get_attempt(
    c: &mut PgConnection,
    id: AttemptId,
) -> Result<Option<AttemptRow>, StorageError> {
    Ok(sqlx::query_as::<_, AttemptRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {ATTEMPT_COLS} FROM attempts WHERE id = $1"
    )))
    .bind(id.0)
    .fetch_optional(c)
    .await?)
}

/// Lock an attempt row.
pub async fn lock_attempt(
    c: &mut PgConnection,
    id: AttemptId,
) -> Result<Option<AttemptRow>, StorageError> {
    Ok(sqlx::query_as::<_, AttemptRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {ATTEMPT_COLS} FROM attempts WHERE id = $1 FOR UPDATE"
    )))
    .bind(id.0)
    .fetch_optional(c)
    .await?)
}

/// Attempts of a task.
pub async fn list_attempts(
    c: &mut PgConnection,
    task: TaskId,
) -> Result<Vec<AttemptRow>, StorageError> {
    Ok(sqlx::query_as::<_, AttemptRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {ATTEMPT_COLS} FROM attempts WHERE task_id = $1 ORDER BY generation"
    )))
    .bind(task.0)
    .fetch_all(c)
    .await?)
}

/// States of the given attempts, `(id, state)`. An id without a row is simply absent.
pub async fn attempt_states(
    c: &mut PgConnection,
    ids: &[Uuid],
) -> Result<Vec<(Uuid, String)>, StorageError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(
        sqlx::query_as::<_, (Uuid, String)>("SELECT id, state FROM attempts WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(c)
            .await?,
    )
}

/// Active attempts of a worker.
pub async fn list_active_attempts_of_worker(
    c: &mut PgConnection,
    worker: WorkerId,
) -> Result<Vec<AttemptRow>, StorageError> {
    Ok(sqlx::query_as::<_, AttemptRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {ATTEMPT_COLS} FROM attempts WHERE worker_id = $1 AND state = 'active' ORDER BY created_at"
    )))
    .bind(worker.0)
    .fetch_all(c)
    .await?)
}

/// Kind and spec of tasks being computed or awaiting their verification, most recent first: what
/// validators will verify next (ADR-0037).
pub async fn list_upcoming_specs(
    c: &mut PgConnection,
    limit: i64,
) -> Result<Vec<(String, Vec<u8>)>, StorageError> {
    Ok(sqlx::query_as::<_, (String, Vec<u8>)>(
        "SELECT compute_kind, spec_cbor FROM tasks WHERE state IN ('assigned', 'submitted') \
         ORDER BY updated_at DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(c)
    .await?)
}

/// Whether every task of these jobs is settled for good (ADR-0040): accepted, failed or cancelled
/// (a rejected or inconclusive task may still be retried, a disputed one awaits a human), with every
/// verification decided and no dispute open.
pub async fn jobs_settled(c: &mut PgConnection, jobs: &[uuid::Uuid]) -> Result<bool, StorageError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT NOT EXISTS (
           SELECT 1 FROM tasks t WHERE t.job_id = ANY($1) AND (
             t.state NOT IN ('accepted', 'failed', 'cancelled')
             OR EXISTS (SELECT 1 FROM verifications v WHERE v.task_id = t.id AND v.state <> 'decided')
             OR EXISTS (SELECT 1 FROM disputes d WHERE d.task_id = t.id AND d.state = 'open')))",
    )
    .bind(jobs)
    .fetch_one(c)
    .await?)
}

/// Update attempt state.
pub async fn update_attempt_state(
    c: &mut PgConnection,
    id: AttemptId,
    state: AttemptState,
) -> Result<(), StorageError> {
    sqlx::query("UPDATE attempts SET state = $2, updated_at = now() WHERE id = $1")
        .bind(id.0)
        .bind(state.as_str())
        .execute(c)
        .await?;
    Ok(())
}

/// Link an attempt to its reservation.
pub async fn set_attempt_reservation(
    c: &mut PgConnection,
    id: AttemptId,
    reservation: ReservationId,
) -> Result<(), StorageError> {
    sqlx::query("UPDATE attempts SET reservation_id = $2 WHERE id = $1")
        .bind(id.0)
        .bind(reservation.0)
        .execute(c)
        .await?;
    Ok(())
}

/// Insert a lease.
pub async fn insert_lease(
    c: &mut PgConnection,
    id: LeaseId,
    attempt: AttemptId,
    expires_at: OffsetDateTime,
    max_until: OffsetDateTime,
    renew_secs: i64,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO leases (id, attempt_id, expires_at, max_until, renew_secs) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id.0)
    .bind(attempt.0)
    .bind(expires_at)
    .bind(max_until)
    .bind(renew_secs as i32)
    .execute(c)
    .await?;
    Ok(())
}

/// Lease of an attempt.
pub async fn get_lease(
    c: &mut PgConnection,
    attempt: AttemptId,
) -> Result<Option<LeaseRow>, StorageError> {
    Ok(sqlx::query_as::<_, LeaseRow>(
        "SELECT id, attempt_id, expires_at, max_until, renewals, released_at FROM leases WHERE attempt_id = $1",
    )
    .bind(attempt.0)
    .fetch_optional(c)
    .await?)
}

/// Renew a still-valid lease by `secs` (capped at `max_until`); returns the new expiry, or `None`
/// if the lease is expired, released or unknown (server clock).
pub async fn renew_lease(
    c: &mut PgConnection,
    attempt: AttemptId,
    secs: i64,
) -> Result<Option<OffsetDateTime>, StorageError> {
    let r: Option<(OffsetDateTime,)> = sqlx::query_as(
        "UPDATE leases SET expires_at = LEAST(max_until, now() + make_interval(secs => COALESCE(renew_secs, $2))), renewals = renewals + 1
         WHERE attempt_id = $1 AND released_at IS NULL AND expires_at > now() RETURNING expires_at",
    )
    .bind(attempt.0)
    .bind(secs as f64)
    .fetch_optional(c)
    .await?;
    Ok(r.map(|(t,)| t))
}

/// Release a lease.
pub async fn release_lease(c: &mut PgConnection, attempt: AttemptId) -> Result<(), StorageError> {
    sqlx::query(
        "UPDATE leases SET released_at = now() WHERE attempt_id = $1 AND released_at IS NULL",
    )
    .bind(attempt.0)
    .execute(c)
    .await?;
    Ok(())
}

/// Whether the lease of an attempt is currently valid (server clock).
pub async fn lease_is_valid(
    c: &mut PgConnection,
    attempt: AttemptId,
) -> Result<bool, StorageError> {
    let r: Option<(bool,)> = sqlx::query_as(
        "SELECT (released_at IS NULL AND expires_at > now()) FROM leases WHERE attempt_id = $1",
    )
    .bind(attempt.0)
    .fetch_optional(c)
    .await?;
    Ok(r.map(|(b,)| b).unwrap_or(false))
}

/// Attempts whose lease expired and that are still active (for the reaper). Locks the attempt rows.
pub async fn expired_active_attempts(
    c: &mut PgConnection,
    limit: i64,
) -> Result<Vec<AttemptRow>, StorageError> {
    Ok(sqlx::query_as::<_, AttemptRow>(sqlx::AssertSqlSafe("SELECT a.id, a.task_id, a.worker_id, a.generation, a.role, a.state, a.assignment_cbor, a.assignment_hash, a.reservation_id, a.created_at
         FROM attempts a JOIN leases l ON l.attempt_id = a.id
         WHERE a.state = 'active' AND l.released_at IS NULL AND l.expires_at <= now()
         ORDER BY l.expires_at LIMIT $1 FOR UPDATE OF a SKIP LOCKED".to_string()))
    .bind(limit)
    .fetch_all(c)
    .await?)
}

/// Insert a budget reservation.
pub async fn insert_reservation(
    c: &mut PgConnection,
    id: ReservationId,
    project: ProjectId,
    task: TaskId,
    attempt: AttemptId,
    amount: Credits,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO budget_reservations (id, project_id, task_id, attempt_id, amount, state) VALUES ($1, $2, $3, $4, $5, 'reserved')",
    )
    .bind(id.0)
    .bind(project.0)
    .bind(task.0)
    .bind(attempt.0)
    .bind(amount.get() as i64)
    .execute(c)
    .await?;
    Ok(())
}

/// Lock the reservation of an attempt.
pub async fn lock_reservation(
    c: &mut PgConnection,
    attempt: AttemptId,
) -> Result<Option<ReservationRow>, StorageError> {
    Ok(sqlx::query_as::<_, ReservationRow>(
        "SELECT id, project_id, task_id, attempt_id, amount, state FROM budget_reservations WHERE attempt_id = $1 FOR UPDATE",
    )
    .bind(attempt.0)
    .fetch_optional(c)
    .await?)
}

/// Resolve a reservation (`committed` or `released`). Returns false if it was not `reserved`.
pub async fn resolve_reservation(
    c: &mut PgConnection,
    id: ReservationId,
    state: &str,
) -> Result<bool, StorageError> {
    let r = sqlx::query("UPDATE budget_reservations SET state = $2, resolved_at = now() WHERE id = $1 AND state = 'reserved'")
        .bind(id.0)
        .bind(state)
        .execute(c)
        .await?;
    Ok(r.rows_affected() == 1)
}

/// Insert a submission (unique per task generation).
pub async fn insert_submission(
    c: &mut PgConnection,
    id: SubmissionId,
    task: TaskId,
    attempt: AttemptId,
    generation: i64,
    hash: &Sha256Digest,
    cbor: &[u8],
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO submissions (id, task_id, attempt_id, generation, submission_hash, submission_cbor) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id.0)
    .bind(task.0)
    .bind(attempt.0)
    .bind(generation)
    .bind(hash.as_bytes().as_slice())
    .bind(cbor)
    .execute(c)
    .await?;
    Ok(())
}

/// Submission of an attempt.
pub async fn get_submission_by_attempt(
    c: &mut PgConnection,
    attempt: AttemptId,
) -> Result<Option<SubmissionRow>, StorageError> {
    Ok(sqlx::query_as::<_, SubmissionRow>(
        "SELECT id, task_id, attempt_id, generation, submission_hash, submission_cbor, created_at FROM submissions WHERE attempt_id = $1",
    )
    .bind(attempt.0)
    .fetch_optional(c)
    .await?)
}

/// Submission by id.
pub async fn get_submission(
    c: &mut PgConnection,
    id: SubmissionId,
) -> Result<Option<SubmissionRow>, StorageError> {
    Ok(sqlx::query_as::<_, SubmissionRow>(
        "SELECT id, task_id, attempt_id, generation, submission_hash, submission_cbor, created_at FROM submissions WHERE id = $1",
    )
    .bind(id.0)
    .fetch_optional(c)
    .await?)
}

/// Create a pending verification for a submission.
pub async fn insert_verification(
    c: &mut PgConnection,
    id: Uuid,
    submission: SubmissionId,
    task: TaskId,
    attempt: AttemptId,
) -> Result<(), StorageError> {
    sqlx::query("INSERT INTO verifications (id, submission_id, task_id, attempt_id, state) VALUES ($1, $2, $3, $4, 'pending')")
        .bind(id)
        .bind(submission.0)
        .bind(task.0)
        .bind(attempt.0)
        .execute(c)
        .await?;
    Ok(())
}

const VERIF_COLS: &str =
    "id, submission_id, task_id, attempt_id, state, validator_id, lease_expires_at, created_at";

/// Verifications `validator` may take, oldest first, locked for this transaction (skipping those
/// another transaction holds). Administrative diversity (ADR-0014): a validator never takes a
/// verification of a submission its own operator already holds or decided. Several keys of one
/// operator are one opinion. An engine check (ADR-0031) is taken only by a validator recomputing
/// on the CPU (`cpu_reference`); it checks the engine, not the operator, so no operator rule applies
/// to it (its CPU recomputation differs by construction from the GPU decision it checks, and the
/// CPU validator that audited the same submission may take it), and it does not count as another
/// operator's opinion for the others. `secondary_only`: a
/// validator that takes only audits and engine checks, never a primary verification (a CPU
/// validator next to GPU validators). Returns
/// `(verification, task, submission)`.
pub async fn lock_verification_candidates(
    c: &mut PgConnection,
    validator: ValidatorId,
    limit: i64,
    cpu_reference: bool,
    secondary_only: bool,
) -> Result<Vec<(Uuid, Uuid, Uuid)>, StorageError> {
    Ok(sqlx::query_as(
        "SELECT v.id, v.task_id, v.submission_id FROM verifications v
           JOIN attempts a ON a.id = v.attempt_id
           JOIN validators me ON me.id = $1
         WHERE a.state = 'submitted'
           AND (v.state = 'pending' OR (v.state = 'in_progress' AND v.lease_expires_at <= now()))
           AND (v.kind <> 'engine_check' OR $3)
           AND (v.kind <> 'primary' OR NOT $4)
           AND (v.kind = 'engine_check' OR NOT EXISTS (
             SELECT 1 FROM verifications other
               JOIN validators ov ON ov.id = other.validator_id
             WHERE other.submission_id = v.submission_id
               AND other.id <> v.id
               AND other.kind <> 'engine_check'
               AND ov.operator_id = me.operator_id
               AND (other.state = 'decided'
                    OR (other.state = 'in_progress' AND other.lease_expires_at > now()))
           ))
         ORDER BY v.created_at LIMIT $2 FOR UPDATE OF v SKIP LOCKED",
    )
    .bind(validator.0)
    .bind(limit)
    .bind(cpu_reference)
    .bind(secondary_only)
    .fetch_all(&mut *c)
    .await?)
}

/// Give a locked candidate verification to `validator`, with a lease.
pub async fn start_verification(
    c: &mut PgConnection,
    id: Uuid,
    validator: ValidatorId,
    lease_secs: i64,
) -> Result<Option<VerificationRow>, StorageError> {
    Ok(
        sqlx::query_as::<_, VerificationRow>(sqlx::AssertSqlSafe(format!(
            "UPDATE verifications SET state = 'in_progress', validator_id = $2,
            lease_expires_at = now() + make_interval(secs => $3), updated_at = now()
         WHERE id = $1 RETURNING {VERIF_COLS}"
        )))
        .bind(id)
        .bind(validator.0)
        .bind(lease_secs as f64)
        .fetch_optional(c)
        .await?,
    )
}

/// Claim the oldest verification `validator` may take.
pub async fn claim_verification(
    c: &mut PgConnection,
    validator: ValidatorId,
    lease_secs: i64,
    cpu_reference: bool,
) -> Result<Option<VerificationRow>, StorageError> {
    let Some((id, _, _)) = lock_verification_candidates(c, validator, 1, cpu_reference, false)
        .await?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    start_verification(c, id, validator, lease_secs).await
}

/// Give a claimed verification back to the queue (only by the validator holding it). Returns
/// whether a row changed.
pub async fn release_verification(
    c: &mut PgConnection,
    id: Uuid,
    validator: ValidatorId,
) -> Result<bool, StorageError> {
    let r = sqlx::query(
        "UPDATE verifications SET state = 'pending', validator_id = NULL, lease_expires_at = NULL, updated_at = now()
         WHERE id = $1 AND validator_id = $2 AND state = 'in_progress'",
    )
    .bind(id)
    .bind(validator.0)
    .execute(c)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Hand every verification a validator holds back to the queue (it was deauthorized): another
/// validator claims them. Returns how many were released.
pub async fn release_all_verifications_of(
    c: &mut PgConnection,
    validator: ValidatorId,
) -> Result<u64, StorageError> {
    let r = sqlx::query(
        "UPDATE verifications SET state = 'pending', validator_id = NULL, lease_expires_at = NULL, updated_at = now()
         WHERE validator_id = $1 AND state = 'in_progress'",
    )
    .bind(validator.0)
    .execute(c)
    .await?;
    Ok(r.rows_affected())
}

/// Verifications currently held by a validator (lease not expired).
pub async fn list_in_progress_verifications(
    c: &mut PgConnection,
    validator: ValidatorId,
) -> Result<Vec<VerificationRow>, StorageError> {
    Ok(sqlx::query_as::<_, VerificationRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {VERIF_COLS} FROM verifications WHERE state = 'in_progress' AND validator_id = $1 AND lease_expires_at > now()"
    )))
    .bind(validator.0)
    .fetch_all(c)
    .await?)
}

/// Lock a verification row.
pub async fn lock_verification(
    c: &mut PgConnection,
    id: Uuid,
) -> Result<Option<VerificationRow>, StorageError> {
    Ok(
        sqlx::query_as::<_, VerificationRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {VERIF_COLS} FROM verifications WHERE id = $1 FOR UPDATE"
        )))
        .bind(id)
        .fetch_optional(c)
        .await?,
    )
}

/// Verification by attempt.
pub async fn get_verification_by_attempt(
    c: &mut PgConnection,
    attempt: AttemptId,
) -> Result<Option<VerificationRow>, StorageError> {
    Ok(
        sqlx::query_as::<_, VerificationRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {VERIF_COLS} FROM verifications WHERE attempt_id = $1"
        )))
        .bind(attempt.0)
        .fetch_optional(c)
        .await?,
    )
}

/// Mark a verification decided.
pub async fn set_verification_decided(c: &mut PgConnection, id: Uuid) -> Result<(), StorageError> {
    sqlx::query("UPDATE verifications SET state = 'decided', updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(c)
        .await?;
    Ok(())
}

/// Insert a decision.
pub async fn insert_decision(
    c: &mut PgConnection,
    id: DecisionId,
    verification: Uuid,
    validator: ValidatorId,
    hash: &Sha256Digest,
    cbor: &[u8],
    outcome: VerificationOutcome,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO decisions (id, verification_id, validator_id, decision_hash, decision_cbor, outcome) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id.0)
    .bind(verification)
    .bind(validator.0)
    .bind(hash.as_bytes().as_slice())
    .bind(cbor)
    .bind(outcome.as_str())
    .execute(c)
    .await?;
    Ok(())
}

/// Decision of a verification.
pub async fn get_decision_by_verification(
    c: &mut PgConnection,
    verification: Uuid,
) -> Result<Option<DecisionRow>, StorageError> {
    Ok(sqlx::query_as::<_, DecisionRow>(
        "SELECT id, verification_id, validator_id, decision_hash, decision_cbor, outcome, created_at FROM decisions WHERE verification_id = $1",
    )
    .bind(verification)
    .fetch_optional(c)
    .await?)
}

/// One decision by id.
pub async fn get_decision(
    c: &mut PgConnection,
    id: Uuid,
) -> Result<Option<DecisionRow>, StorageError> {
    Ok(sqlx::query_as::<_, DecisionRow>(
        "SELECT id, verification_id, validator_id, decision_hash, decision_cbor, outcome, created_at FROM decisions WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(c)
    .await?)
}

/// Insert a receipt.
#[allow(clippy::too_many_arguments)]
pub async fn insert_receipt(
    c: &mut PgConnection,
    id: ReceiptId,
    attempt: AttemptId,
    worker: WorkerId,
    project: ProjectId,
    role: AttemptRole,
    amount: Credits,
    ledger_txn: Option<LedgerTxnId>,
    hash: &Sha256Digest,
    cbor: &[u8],
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO receipts (id, attempt_id, worker_id, project_id, role, amount, reward_state, ledger_txn_id, receipt_hash, receipt_cbor)
         VALUES ($1, $2, $3, $4, $5, $6, 'earned', $7, $8, $9)",
    )
    .bind(id.0)
    .bind(attempt.0)
    .bind(worker.0)
    .bind(project.0)
    .bind(role.as_str())
    .bind(amount.get() as i64)
    .bind(ledger_txn.map(|t| t.0))
    .bind(hash.as_bytes().as_slice())
    .bind(cbor)
    .execute(c)
    .await?;
    Ok(())
}

const RECEIPT_COLS: &str = "id, attempt_id, worker_id, project_id, role, amount, reward_state, ledger_txn_id, receipt_hash, receipt_cbor, created_at";

/// Receipts of a worker.
pub async fn list_receipts_of_worker(
    c: &mut PgConnection,
    worker: WorkerId,
    limit: i64,
) -> Result<Vec<ReceiptRow>, StorageError> {
    Ok(sqlx::query_as::<_, ReceiptRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {RECEIPT_COLS} FROM receipts WHERE worker_id = $1 ORDER BY created_at DESC LIMIT $2"
    )))
    .bind(worker.0)
    .bind(limit)
    .fetch_all(c)
    .await?)
}

/// Receipts of a project.
pub async fn list_receipts_of_project(
    c: &mut PgConnection,
    project: ProjectId,
    limit: i64,
) -> Result<Vec<ReceiptRow>, StorageError> {
    Ok(sqlx::query_as::<_, ReceiptRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {RECEIPT_COLS} FROM receipts WHERE project_id = $1 ORDER BY created_at DESC LIMIT $2"
    )))
    .bind(project.0)
    .bind(limit)
    .fetch_all(c)
    .await?)
}

/// Receipt of an attempt/role.
pub async fn get_receipt(
    c: &mut PgConnection,
    attempt: AttemptId,
    role: AttemptRole,
) -> Result<Option<ReceiptRow>, StorageError> {
    Ok(sqlx::query_as::<_, ReceiptRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {RECEIPT_COLS} FROM receipts WHERE attempt_id = $1 AND role = $2"
    )))
    .bind(attempt.0)
    .bind(role.as_str())
    .fetch_optional(c)
    .await?)
}

/// Receipt by id.
pub async fn get_receipt_by_id(
    c: &mut PgConnection,
    id: ReceiptId,
) -> Result<Option<ReceiptRow>, StorageError> {
    Ok(sqlx::query_as::<_, ReceiptRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {RECEIPT_COLS} FROM receipts WHERE id = $1"
    )))
    .bind(id.0)
    .fetch_optional(c)
    .await?)
}
