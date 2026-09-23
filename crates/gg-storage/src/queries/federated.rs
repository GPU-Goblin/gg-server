//! Validator operators, audit verifications and disputes (ADR-0014).

use ::time::OffsetDateTime;
use gg_core::*;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::StorageError;

/// Operator that administers one or more validators.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct OperatorRow {
    /// Id.
    pub id: Uuid,
    /// Unique name.
    pub name: String,
    /// Contact (no secrets).
    pub contact: String,
    /// Whether it may validate.
    pub active: bool,
    /// Created.
    pub created_at: OffsetDateTime,
}

/// Create an operator.
pub async fn insert_operator(
    c: &mut PgConnection,
    id: Uuid,
    name: &str,
    contact: &str,
) -> Result<(), StorageError> {
    sqlx::query("INSERT INTO validator_operators (id, name, contact) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(name)
        .bind(contact)
        .execute(c)
        .await?;
    Ok(())
}

/// All operators, oldest first.
pub async fn list_operators(c: &mut PgConnection) -> Result<Vec<OperatorRow>, StorageError> {
    Ok(sqlx::query_as::<_, OperatorRow>(
        "SELECT id, name, contact, active, created_at FROM validator_operators ORDER BY created_at",
    )
    .fetch_all(c)
    .await?)
}

/// One operator by name.
pub async fn get_operator_by_name(
    c: &mut PgConnection,
    name: &str,
) -> Result<Option<OperatorRow>, StorageError> {
    Ok(sqlx::query_as::<_, OperatorRow>(
        "SELECT id, name, contact, active, created_at FROM validator_operators WHERE name = $1",
    )
    .bind(name)
    .fetch_optional(c)
    .await?)
}

/// Operator of a validator.
pub async fn operator_of_validator(
    c: &mut PgConnection,
    validator: ValidatorId,
) -> Result<Option<Uuid>, StorageError> {
    Ok(
        sqlx::query_as::<_, (Uuid,)>("SELECT operator_id FROM validators WHERE id = $1")
            .bind(validator.0)
            .fetch_optional(c)
            .await?
            .map(|(o,)| o),
    )
}

/// Attach a validator to an operator.
pub async fn set_validator_operator(
    c: &mut PgConnection,
    validator: ValidatorId,
    operator: Uuid,
) -> Result<(), StorageError> {
    sqlx::query("UPDATE validators SET operator_id = $2 WHERE id = $1")
        .bind(validator.0)
        .bind(operator)
        .execute(c)
        .await?;
    Ok(())
}

/// How many operators can currently validate (active, with at least one authorized validator).
pub async fn active_operator_count(c: &mut PgConnection) -> Result<i64, StorageError> {
    Ok(sqlx::query_as::<_, (i64,)>(
        "SELECT count(DISTINCT v.operator_id) FROM validators v
         JOIN validator_operators o ON o.id = v.operator_id
         WHERE v.authorized AND o.active",
    )
    .fetch_one(c)
    .await?
    .0)
}

/// Verifications of one submission with the deciding validator and its operator.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct VerificationSummary {
    /// Verification id.
    pub id: Uuid,
    /// `primary` or `audit`.
    pub kind: String,
    /// `pending`, `in_progress` or `decided`.
    pub state: String,
    /// Validator that holds or decided it.
    pub validator_id: Option<Uuid>,
    /// Operator of that validator.
    pub operator_id: Option<Uuid>,
    /// Decision id, when decided.
    pub decision_id: Option<Uuid>,
    /// Outcome, when decided.
    pub outcome: Option<String>,
}

/// Every verification of a submission, with decisions when they exist.
pub async fn verifications_of_submission(
    c: &mut PgConnection,
    submission: SubmissionId,
) -> Result<Vec<VerificationSummary>, StorageError> {
    Ok(sqlx::query_as::<_, VerificationSummary>(
        "SELECT v.id, v.kind, v.state, v.validator_id, val.operator_id, d.id AS decision_id, d.outcome
         FROM verifications v
         LEFT JOIN validators val ON val.id = v.validator_id
         LEFT JOIN decisions d ON d.verification_id = v.id
         WHERE v.submission_id = $1
         ORDER BY v.kind",
    )
    .bind(submission.0)
    .fetch_all(c)
    .await?)
}

/// Open the engine check of a submission (ADR-0031): a verification only a validator recomputing
/// on the CPU may take. Returns whether one was created (at most one per submission).
pub async fn insert_engine_check_verification(
    c: &mut PgConnection,
    id: Uuid,
    submission: SubmissionId,
    task: TaskId,
    attempt: AttemptId,
) -> Result<bool, StorageError> {
    let r = sqlx::query(
        "INSERT INTO verifications (id, submission_id, task_id, attempt_id, state, kind)
         VALUES ($1, $2, $3, $4, 'pending', 'engine_check')
         ON CONFLICT (submission_id, kind) DO NOTHING",
    )
    .bind(id)
    .bind(submission.0)
    .bind(task.0)
    .bind(attempt.0)
    .execute(c)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Kind of a verification: `primary`, `audit` or `engine_check`.
pub async fn verification_kind(c: &mut PgConnection, id: Uuid) -> Result<String, StorageError> {
    Ok(
        sqlx::query_scalar("SELECT kind FROM verifications WHERE id = $1")
            .bind(id)
            .fetch_one(c)
            .await?,
    )
}

/// Create an audit verification for a submission (idempotent through the unique constraint).
pub async fn insert_audit_verification(
    c: &mut PgConnection,
    id: Uuid,
    submission: SubmissionId,
    task: TaskId,
    attempt: AttemptId,
) -> Result<bool, StorageError> {
    let r = sqlx::query(
        "INSERT INTO verifications (id, submission_id, task_id, attempt_id, state, kind)
         VALUES ($1, $2, $3, $4, 'pending', 'audit')
         ON CONFLICT (submission_id, kind) DO NOTHING",
    )
    .bind(id)
    .bind(submission.0)
    .bind(task.0)
    .bind(attempt.0)
    .execute(c)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Open dispute row.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct DisputeRow {
    /// Id.
    pub id: Uuid,
    /// Submission.
    pub submission_id: Uuid,
    /// Task.
    pub task_id: Uuid,
    /// Attempt.
    pub attempt_id: Uuid,
    /// `open` or `resolved`.
    pub state: String,
    /// Decisions that disagreed.
    pub outcomes: serde_json::Value,
    /// Resolution when resolved.
    pub resolution: Option<String>,
    /// Who resolved it.
    pub resolved_by: String,
    /// Reason recorded by the operator.
    pub reason: String,
    /// Created.
    pub created_at: OffsetDateTime,
    /// Resolved.
    pub resolved_at: Option<OffsetDateTime>,
}

const DISPUTE_COLS: &str = "id, submission_id, task_id, attempt_id, state, outcomes, resolution, resolved_by, reason, created_at, resolved_at";

/// Record a disagreement.
pub async fn insert_dispute(
    c: &mut PgConnection,
    id: Uuid,
    submission: SubmissionId,
    task: TaskId,
    attempt: AttemptId,
    outcomes: &serde_json::Value,
) -> Result<bool, StorageError> {
    let r = sqlx::query(
        "INSERT INTO disputes (id, submission_id, task_id, attempt_id, state, outcomes)
         VALUES ($1, $2, $3, $4, 'open', $5) ON CONFLICT (submission_id) DO NOTHING",
    )
    .bind(id)
    .bind(submission.0)
    .bind(task.0)
    .bind(attempt.0)
    .bind(outcomes)
    .execute(c)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Lock a dispute for resolution.
pub async fn lock_dispute(
    c: &mut PgConnection,
    id: Uuid,
) -> Result<Option<DisputeRow>, StorageError> {
    Ok(sqlx::query_as::<_, DisputeRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {DISPUTE_COLS} FROM disputes WHERE id = $1 FOR UPDATE"
    )))
    .bind(id)
    .fetch_optional(c)
    .await?)
}

/// Mark a dispute resolved.
pub async fn resolve_dispute(
    c: &mut PgConnection,
    id: Uuid,
    resolution: VerificationOutcome,
    resolved_by: &str,
    reason: &str,
) -> Result<(), StorageError> {
    sqlx::query(
        "UPDATE disputes SET state = 'resolved', resolution = $2, resolved_by = $3, reason = $4,
         resolved_at = now() WHERE id = $1 AND state = 'open'",
    )
    .bind(id)
    .bind(resolution.as_str())
    .bind(resolved_by)
    .bind(reason)
    .execute(c)
    .await?;
    Ok(())
}

/// Disputes, open ones first.
pub async fn list_disputes(
    c: &mut PgConnection,
    only_open: bool,
    limit: i64,
) -> Result<Vec<DisputeRow>, StorageError> {
    let sql = if only_open {
        format!(
            "SELECT {DISPUTE_COLS} FROM disputes WHERE state = 'open' ORDER BY created_at LIMIT $1"
        )
    } else {
        format!("SELECT {DISPUTE_COLS} FROM disputes ORDER BY created_at DESC LIMIT $1")
    };
    Ok(sqlx::query_as::<_, DisputeRow>(sqlx::AssertSqlSafe(sql))
        .bind(limit)
        .fetch_all(c)
        .await?)
}
