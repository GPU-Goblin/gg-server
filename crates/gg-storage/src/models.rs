//! Row types. Text states are parsed into `gg-core` enums by accessor methods.

use ::time::OffsetDateTime;
use gg_core::*;
use sqlx::FromRow;
use uuid::Uuid;

use crate::StorageError;

fn parse<T: std::str::FromStr>(s: &str, what: &str) -> Result<T, StorageError> {
    s.parse()
        .map_err(|_| StorageError::Corrupt(format!("{what}: {s}")))
}

fn digest(b: &[u8]) -> Result<Sha256Digest, StorageError> {
    let arr: [u8; 32] = b
        .try_into()
        .map_err(|_| StorageError::Corrupt("digest length".into()))?;
    Ok(Sha256Digest(arr))
}

/// Researcher.
#[derive(Clone, Debug, FromRow)]
pub struct ResearcherRow {
    /// Id.
    pub id: Uuid,
    /// Display name.
    pub display_name: String,
    /// ORCID iD if linked.
    pub orcid: Option<String>,
    /// Admission state text.
    pub admission_state: String,
    /// Admission note.
    pub admission_note: String,
    /// Created.
    pub created_at: OffsetDateTime,
}

impl ResearcherRow {
    /// Admission state.
    pub fn admission(&self) -> Result<AdmissionState, StorageError> {
        parse(&self.admission_state, "admission_state")
    }
}

/// API token lookup result.
#[derive(Clone, Debug, FromRow)]
pub struct TokenRow {
    /// Role: admin or researcher.
    pub role: String,
    /// Researcher id for researcher tokens.
    pub researcher_id: Option<Uuid>,
    /// Label.
    pub label: String,
    /// Expiry.
    pub expires_at: Option<OffsetDateTime>,
    /// Revocation.
    pub revoked_at: Option<OffsetDateTime>,
}

/// Project.
#[derive(Clone, Debug, FromRow)]
pub struct ProjectRow {
    /// Id.
    pub id: Uuid,
    /// Name.
    pub name: String,
    /// State text.
    pub state: String,
    /// Allowed compute kinds.
    pub allowed_kinds: Vec<String>,
    /// Created.
    pub created_at: OffsetDateTime,
}

impl ProjectRow {
    /// State.
    pub fn project_state(&self) -> Result<ProjectState, StorageError> {
        parse(&self.state, "project state")
    }
}

/// Budget balances.
#[derive(Clone, Debug, FromRow)]
pub struct BudgetRow {
    /// Project.
    pub project_id: Uuid,
    /// Available.
    pub available: i64,
    /// Reserved.
    pub reserved: i64,
    /// Spent.
    pub spent: i64,
}

/// Worker.
#[derive(Clone, Debug, FromRow)]
pub struct WorkerRow {
    /// Id.
    pub id: Uuid,
    /// Public key bytes.
    pub public_key: Vec<u8>,
    /// Name.
    pub display_name: String,
    /// Authorized by an operator.
    pub authorized: bool,
    /// Payout address (P7).
    pub payout_address: Option<String>,
    /// Created.
    pub created_at: OffsetDateTime,
}

/// Validator.
#[derive(Clone, Debug, FromRow)]
pub struct ValidatorRow {
    /// Id.
    pub id: Uuid,
    /// Public key bytes.
    pub public_key: Vec<u8>,
    /// Name.
    pub display_name: String,
    /// Authorized.
    pub authorized: bool,
    /// Created.
    pub created_at: OffsetDateTime,
}

/// Session lookup result.
#[derive(Clone, Debug, FromRow)]
pub struct SessionRow {
    /// `worker` or `validator`.
    pub principal_kind: String,
    /// Principal id.
    pub principal_id: Uuid,
    /// Expiry.
    pub expires_at: OffsetDateTime,
    /// Revocation.
    pub revoked_at: Option<OffsetDateTime>,
}

/// Artifact metadata.
#[derive(Clone, Debug, FromRow)]
pub struct ArtifactRow {
    /// Hash bytes.
    pub hash: Vec<u8>,
    /// Size.
    pub size: i64,
    /// Media type.
    pub media_type: String,
    /// Schema label.
    pub schema: String,
    /// Visibility.
    pub visibility: String,
    /// License.
    pub license: String,
    /// Owner project.
    pub project_id: Option<Uuid>,
    /// Created.
    pub created_at: OffsetDateTime,
}

impl ArtifactRow {
    /// Digest.
    pub fn digest(&self) -> Result<Sha256Digest, StorageError> {
        digest(&self.hash)
    }
}

/// Experiment.
#[derive(Clone, Debug, FromRow)]
pub struct ExperimentRow {
    /// Id.
    pub id: Uuid,
    /// Project.
    pub project_id: Uuid,
    /// Name.
    pub name: String,
    /// Created.
    pub created_at: OffsetDateTime,
}

/// Job.
#[derive(Clone, Debug, FromRow)]
pub struct JobRow {
    /// Id.
    pub id: Uuid,
    /// Project.
    pub project_id: Uuid,
    /// Experiment.
    pub experiment_id: Uuid,
    /// Submitter.
    pub submitted_by: Option<Uuid>,
    /// Name.
    pub name: String,
    /// Cancelled flag.
    pub cancelled: bool,
    /// Created.
    pub created_at: OffsetDateTime,
}

/// Task.
#[derive(Clone, Debug, FromRow)]
pub struct TaskRow {
    /// Id.
    pub id: Uuid,
    /// Job.
    pub job_id: Uuid,
    /// Project.
    pub project_id: Uuid,
    /// Experiment.
    pub experiment_id: Uuid,
    /// Index in job.
    pub index_in_job: i32,
    /// Compute kind.
    pub compute_kind: String,
    /// Spec hash bytes.
    pub spec_hash: Vec<u8>,
    /// Definition hash bytes.
    pub definition_hash: Vec<u8>,
    /// Canonical spec bytes.
    pub spec_cbor: Vec<u8>,
    /// State text.
    pub state: String,
    /// Generation.
    pub generation: i64,
    /// Attempts made.
    pub attempts_made: i32,
    /// Max attempts.
    pub max_attempts: i32,
    /// Priority.
    pub priority: i32,
    /// Primary reward.
    pub reward_primary: i64,
    /// Verification cost.
    pub verification_cost: i64,
    /// Estimated CPU seconds.
    pub est_cpu_secs: f64,
    /// Created.
    pub created_at: OffsetDateTime,
    /// Updated.
    pub updated_at: OffsetDateTime,
}

impl TaskRow {
    /// State.
    pub fn task_state(&self) -> Result<TaskState, StorageError> {
        parse(&self.state, "task state")
    }
    /// Spec hash.
    pub fn spec_digest(&self) -> Result<Sha256Digest, StorageError> {
        digest(&self.spec_hash)
    }
    /// Definition hash.
    pub fn definition_digest(&self) -> Result<Sha256Digest, StorageError> {
        digest(&self.definition_hash)
    }
}

/// Attempt.
#[derive(Clone, Debug, FromRow)]
pub struct AttemptRow {
    /// Id.
    pub id: Uuid,
    /// Task.
    pub task_id: Uuid,
    /// Worker.
    pub worker_id: Uuid,
    /// Generation.
    pub generation: i64,
    /// Role text.
    pub role: String,
    /// State text.
    pub state: String,
    /// Signed assignment bytes.
    pub assignment_cbor: Vec<u8>,
    /// Assignment hash.
    pub assignment_hash: Vec<u8>,
    /// Reservation.
    pub reservation_id: Option<Uuid>,
    /// Created.
    pub created_at: OffsetDateTime,
}

impl AttemptRow {
    /// State.
    pub fn attempt_state(&self) -> Result<AttemptState, StorageError> {
        parse(&self.state, "attempt state")
    }
    /// Role.
    pub fn attempt_role(&self) -> Result<AttemptRole, StorageError> {
        parse(&self.role, "attempt role")
    }
    /// Assignment hash.
    pub fn assignment_digest(&self) -> Result<Sha256Digest, StorageError> {
        digest(&self.assignment_hash)
    }
}

/// Lease.
#[derive(Clone, Debug, FromRow)]
pub struct LeaseRow {
    /// Id.
    pub id: Uuid,
    /// Attempt.
    pub attempt_id: Uuid,
    /// Expiry.
    pub expires_at: OffsetDateTime,
    /// Hard bound.
    pub max_until: OffsetDateTime,
    /// Renewals.
    pub renewals: i32,
    /// Released.
    pub released_at: Option<OffsetDateTime>,
}

/// Budget reservation.
#[derive(Clone, Debug, FromRow)]
pub struct ReservationRow {
    /// Id.
    pub id: Uuid,
    /// Project.
    pub project_id: Uuid,
    /// Task.
    pub task_id: Uuid,
    /// Attempt.
    pub attempt_id: Uuid,
    /// Amount.
    pub amount: i64,
    /// State: reserved, committed, released.
    pub state: String,
}

/// Submission.
#[derive(Clone, Debug, FromRow)]
pub struct SubmissionRow {
    /// Id.
    pub id: Uuid,
    /// Task.
    pub task_id: Uuid,
    /// Attempt.
    pub attempt_id: Uuid,
    /// Generation.
    pub generation: i64,
    /// Hash.
    pub submission_hash: Vec<u8>,
    /// Signed bytes.
    pub submission_cbor: Vec<u8>,
    /// Created.
    pub created_at: OffsetDateTime,
}

impl SubmissionRow {
    /// Hash.
    pub fn digest(&self) -> Result<Sha256Digest, StorageError> {
        digest(&self.submission_hash)
    }
}

/// Verification work item.
#[derive(Clone, Debug, FromRow)]
pub struct VerificationRow {
    /// Id.
    pub id: Uuid,
    /// Submission.
    pub submission_id: Uuid,
    /// Task.
    pub task_id: Uuid,
    /// Attempt.
    pub attempt_id: Uuid,
    /// State text.
    pub state: String,
    /// Validator holding it.
    pub validator_id: Option<Uuid>,
    /// Lease expiry.
    pub lease_expires_at: Option<OffsetDateTime>,
    /// Created.
    pub created_at: OffsetDateTime,
}

impl VerificationRow {
    /// State.
    pub fn verification_state(&self) -> Result<VerificationState, StorageError> {
        parse(&self.state, "verification state")
    }
}

/// Decision.
#[derive(Clone, Debug, FromRow)]
pub struct DecisionRow {
    /// Id.
    pub id: Uuid,
    /// Verification.
    pub verification_id: Uuid,
    /// Validator.
    pub validator_id: Uuid,
    /// Hash.
    pub decision_hash: Vec<u8>,
    /// Signed bytes.
    pub decision_cbor: Vec<u8>,
    /// Outcome text.
    pub outcome: String,
    /// Created.
    pub created_at: OffsetDateTime,
}

impl DecisionRow {
    /// Outcome.
    pub fn verification_outcome(&self) -> Result<VerificationOutcome, StorageError> {
        parse(&self.outcome, "outcome")
    }
    /// Hash.
    pub fn digest(&self) -> Result<Sha256Digest, StorageError> {
        digest(&self.decision_hash)
    }
}

/// Receipt.
#[derive(Clone, Debug, FromRow)]
pub struct ReceiptRow {
    /// Id.
    pub id: Uuid,
    /// Attempt.
    pub attempt_id: Uuid,
    /// Worker.
    pub worker_id: Uuid,
    /// Project.
    pub project_id: Uuid,
    /// Role.
    pub role: String,
    /// Amount.
    pub amount: i64,
    /// Reward state.
    pub reward_state: String,
    /// Ledger transaction.
    pub ledger_txn_id: Option<Uuid>,
    /// Hash.
    pub receipt_hash: Vec<u8>,
    /// Signed bytes.
    pub receipt_cbor: Vec<u8>,
    /// Created.
    pub created_at: OffsetDateTime,
}

/// Idempotency record.
#[derive(Clone, Debug, FromRow)]
pub struct IdempotencyRow {
    /// Request hash.
    pub request_hash: Vec<u8>,
    /// HTTP status.
    pub status: i32,
    /// Stored response.
    pub response: serde_json::Value,
}

/// Outbox event.
#[derive(Clone, Debug, FromRow)]
pub struct OutboxRow {
    /// Id.
    pub id: i64,
    /// Topic.
    pub topic: String,
    /// Payload.
    pub payload: serde_json::Value,
    /// Created.
    pub created_at: OffsetDateTime,
}
