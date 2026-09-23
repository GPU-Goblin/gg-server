//! Experiments, jobs and task rows.

use gg_core::*;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::StorageError;
use crate::models::*;

/// Create an experiment.
pub async fn create_experiment(
    c: &mut PgConnection,
    id: ExperimentId,
    project: ProjectId,
    name: &str,
    created_by: Option<ResearcherId>,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO experiments (id, project_id, name, created_by) VALUES ($1, $2, $3, $4)",
    )
    .bind(id.0)
    .bind(project.0)
    .bind(name)
    .bind(created_by.map(|r| r.0))
    .execute(c)
    .await?;
    Ok(())
}

/// Get an experiment.
pub async fn get_experiment(
    c: &mut PgConnection,
    id: ExperimentId,
) -> Result<Option<ExperimentRow>, StorageError> {
    Ok(sqlx::query_as::<_, ExperimentRow>(
        "SELECT id, project_id, name, created_at FROM experiments WHERE id = $1",
    )
    .bind(id.0)
    .fetch_optional(c)
    .await?)
}

/// Create a job.
pub async fn create_job(
    c: &mut PgConnection,
    id: JobId,
    project: ProjectId,
    experiment: ExperimentId,
    submitted_by: Option<ResearcherId>,
    name: &str,
) -> Result<(), StorageError> {
    sqlx::query("INSERT INTO jobs (id, project_id, experiment_id, submitted_by, name) VALUES ($1, $2, $3, $4, $5)")
        .bind(id.0)
        .bind(project.0)
        .bind(experiment.0)
        .bind(submitted_by.map(|r| r.0))
        .bind(name)
        .execute(c)
        .await?;
    Ok(())
}

/// Get a job.
pub async fn get_job(c: &mut PgConnection, id: JobId) -> Result<Option<JobRow>, StorageError> {
    Ok(sqlx::query_as::<_, JobRow>(
        "SELECT id, project_id, experiment_id, submitted_by, name, cancelled, created_at FROM jobs WHERE id = $1",
    )
    .bind(id.0)
    .fetch_optional(c)
    .await?)
}

/// Lock a job row for update.
pub async fn lock_job(c: &mut PgConnection, id: JobId) -> Result<Option<JobRow>, StorageError> {
    Ok(sqlx::query_as::<_, JobRow>(
        "SELECT id, project_id, experiment_id, submitted_by, name, cancelled, created_at FROM jobs WHERE id = $1 FOR UPDATE",
    )
    .bind(id.0)
    .fetch_optional(c)
    .await?)
}

/// List jobs of a project.
pub async fn list_jobs(
    c: &mut PgConnection,
    project: ProjectId,
    limit: i64,
) -> Result<Vec<JobRow>, StorageError> {
    Ok(sqlx::query_as::<_, JobRow>(
        "SELECT id, project_id, experiment_id, submitted_by, name, cancelled, created_at FROM jobs
         WHERE project_id = $1 ORDER BY created_at DESC LIMIT $2",
    )
    .bind(project.0)
    .bind(limit)
    .fetch_all(c)
    .await?)
}

/// Mark a job cancelled.
pub async fn set_job_cancelled(c: &mut PgConnection, id: JobId) -> Result<(), StorageError> {
    sqlx::query("UPDATE jobs SET cancelled = true, updated_at = now() WHERE id = $1")
        .bind(id.0)
        .execute(c)
        .await?;
    Ok(())
}

/// Parameters for inserting a task.
pub struct NewTask<'a> {
    /// Task id.
    pub id: TaskId,
    /// Job.
    pub job_id: JobId,
    /// Project.
    pub project_id: ProjectId,
    /// Experiment.
    pub experiment_id: ExperimentId,
    /// Index in job.
    pub index: i32,
    /// Compute kind.
    pub compute_kind: &'a str,
    /// Spec hash.
    pub spec_hash: Sha256Digest,
    /// Definition hash.
    pub definition_hash: Sha256Digest,
    /// Canonical spec bytes.
    pub spec_cbor: &'a [u8],
    /// Max attempts.
    pub max_attempts: i32,
    /// Priority.
    pub priority: i32,
    /// Primary reward.
    pub reward_primary: Credits,
    /// Verification cost.
    pub verification_cost: Credits,
    /// Estimated CPU seconds.
    pub est_cpu_secs: f64,
    /// What a worker needs to run it.
    pub requirements: TaskRequirements,
}

/// What a worker needs to run a task (C1). `Default` asks for nothing beyond the base profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskRequirements {
    /// Host memory, bytes.
    pub ram_bytes: i64,
    /// Accelerator memory (host memory for a CPU engine), bytes; 0 when not estimated.
    pub device_bytes: i64,
    /// Minimal `gg-ops/N` profile level.
    pub profile: i32,
}

impl Default for TaskRequirements {
    fn default() -> Self {
        Self {
            ram_bytes: 0,
            device_bytes: 0,
            profile: 1,
        }
    }
}

/// What a worker announces when it claims (C1). `None` everywhere is a worker of an earlier
/// version: it is only filtered on the base profile, as before.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkerCapabilities {
    /// Host memory available to the executor, bytes.
    pub ram_bytes: Option<i64>,
    /// Accelerator memory available to the executor, bytes.
    pub device_bytes: Option<i64>,
    /// `gg-ops/N` level understood.
    pub profile: i32,
    /// Engine (`cpu`, `gpu:0`…), informative.
    pub engine: Option<String>,
    /// Compute kinds the worker claims.
    pub kinds: Vec<String>,
}

/// Insert a queued task.
pub async fn insert_task(c: &mut PgConnection, t: &NewTask<'_>) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO tasks (id, job_id, project_id, experiment_id, index_in_job, compute_kind, spec_hash, definition_hash,
            spec_cbor, state, max_attempts, priority, reward_primary, verification_cost, est_cpu_secs,
            required_ram_bytes, required_device_bytes, required_profile)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'queued', $10, $11, $12, $13, $14, $15, $16, $17)",
    )
    .bind(t.id.0)
    .bind(t.job_id.0)
    .bind(t.project_id.0)
    .bind(t.experiment_id.0)
    .bind(t.index)
    .bind(t.compute_kind)
    .bind(t.spec_hash.as_bytes().as_slice())
    .bind(t.definition_hash.as_bytes().as_slice())
    .bind(t.spec_cbor)
    .bind(t.max_attempts)
    .bind(t.priority)
    .bind(t.reward_primary.get() as i64)
    .bind(t.verification_cost.get() as i64)
    .bind(t.est_cpu_secs)
    .bind(t.requirements.ram_bytes)
    .bind(t.requirements.device_bytes)
    .bind(t.requirements.profile)
    .execute(c)
    .await?;
    Ok(())
}

const TASK_COLS: &str = "id, job_id, project_id, experiment_id, index_in_job, compute_kind, spec_hash, definition_hash, spec_cbor, state, generation, attempts_made, max_attempts, priority, reward_primary, verification_cost, est_cpu_secs, created_at, updated_at";

/// Get a task.
pub async fn get_task(c: &mut PgConnection, id: TaskId) -> Result<Option<TaskRow>, StorageError> {
    Ok(sqlx::query_as::<_, TaskRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {TASK_COLS} FROM tasks WHERE id = $1"
    )))
    .bind(id.0)
    .fetch_optional(c)
    .await?)
}

/// Get a task with a row lock.
pub async fn lock_task(c: &mut PgConnection, id: TaskId) -> Result<Option<TaskRow>, StorageError> {
    Ok(sqlx::query_as::<_, TaskRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {TASK_COLS} FROM tasks WHERE id = $1 FOR UPDATE"
    )))
    .bind(id.0)
    .fetch_optional(c)
    .await?)
}

/// Tasks of a job.
pub async fn list_tasks(c: &mut PgConnection, job: JobId) -> Result<Vec<TaskRow>, StorageError> {
    Ok(sqlx::query_as::<_, TaskRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {TASK_COLS} FROM tasks WHERE job_id = $1 ORDER BY index_in_job"
    )))
    .bind(job.0)
    .fetch_all(c)
    .await?)
}

/// Lock all tasks of a job (for cancellation).
pub async fn lock_tasks_of_job(
    c: &mut PgConnection,
    job: JobId,
) -> Result<Vec<TaskRow>, StorageError> {
    Ok(sqlx::query_as::<_, TaskRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {TASK_COLS} FROM tasks WHERE job_id = $1 ORDER BY index_in_job FOR UPDATE"
    )))
    .bind(job.0)
    .fetch_all(c)
    .await?)
}

/// Select one assignable task compatible with `kinds` and with what the worker announced (memory,
/// profile), skipping rows locked by concurrent claims, rows in `exclude` (already examined in this
/// transaction) and tasks on which `worker` already had an attempt rejected by a validator (a
/// rejected worker never gets the same task again).
pub async fn claim_candidate(
    c: &mut PgConnection,
    worker: WorkerId,
    kinds: &[String],
    exclude: &[Uuid],
    caps: &WorkerCapabilities,
) -> Result<Option<TaskRow>, StorageError> {
    Ok(sqlx::query_as::<_, TaskRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {TASK_COLS} FROM tasks t WHERE t.state = 'queued' AND t.compute_kind = ANY($1) AND NOT (t.id = ANY($2))
           AND NOT EXISTS (SELECT 1 FROM attempts a WHERE a.task_id = t.id AND a.worker_id = $3 AND a.state = 'rejected')
           AND ($4::bigint IS NULL OR t.required_ram_bytes <= $4)
           AND ($5::bigint IS NULL OR t.required_device_bytes <= $5)
           AND t.required_profile <= $6
         ORDER BY t.priority DESC, t.created_at ASC LIMIT 1 FOR UPDATE OF t SKIP LOCKED"
    )))
    .bind(kinds)
    .bind(exclude)
    .bind(worker.0)
    .bind(caps.ram_bytes)
    .bind(caps.device_bytes)
    .bind(caps.profile)
    .fetch_optional(c)
    .await?)
}

/// Queued tasks of `kinds` that no announced capability can run, with the largest requirement:
/// the answer to "why is nothing claimed?".
pub async fn unclaimable_for(
    c: &mut PgConnection,
    kinds: &[String],
    caps: &WorkerCapabilities,
) -> Result<i64, StorageError> {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM tasks t WHERE t.state = 'queued' AND t.compute_kind = ANY($1)
           AND NOT (($2::bigint IS NULL OR t.required_ram_bytes <= $2)
                AND ($3::bigint IS NULL OR t.required_device_bytes <= $3)
                AND t.required_profile <= $4)",
    )
    .bind(kinds)
    .bind(caps.ram_bytes)
    .bind(caps.device_bytes)
    .bind(caps.profile)
    .fetch_one(c)
    .await?;
    Ok(n)
}

/// Update task state (and optionally bump generation / attempts).
pub async fn update_task_state(
    c: &mut PgConnection,
    id: TaskId,
    state: TaskState,
) -> Result<(), StorageError> {
    sqlx::query("UPDATE tasks SET state = $2, updated_at = now() WHERE id = $1")
        .bind(id.0)
        .bind(state.as_str())
        .execute(c)
        .await?;
    Ok(())
}

/// Move a task to `assigned`, bumping generation and attempts; returns the new generation.
pub async fn assign_task(c: &mut PgConnection, id: TaskId) -> Result<i64, StorageError> {
    let (generation,): (i64,) = sqlx::query_as(
        "UPDATE tasks SET state = 'assigned', generation = generation + 1, attempts_made = attempts_made + 1, updated_at = now()
         WHERE id = $1 AND state = 'queued' RETURNING generation",
    )
    .bind(id.0)
    .fetch_one(c)
    .await?;
    Ok(generation)
}

/// Task states of a job (for job state derivation).
pub async fn task_states(c: &mut PgConnection, job: JobId) -> Result<Vec<TaskState>, StorageError> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT state FROM tasks WHERE job_id = $1")
        .bind(job.0)
        .fetch_all(c)
        .await?;
    rows.into_iter()
        .map(|(s,)| {
            s.parse::<TaskState>()
                .map_err(|e| StorageError::Corrupt(e.to_string()))
        })
        .collect()
}

/// Count tasks per state (admin/metrics).
pub async fn count_tasks_by_state(
    c: &mut PgConnection,
) -> Result<Vec<(String, i64)>, StorageError> {
    Ok(
        sqlx::query_as("SELECT state, count(*) FROM tasks GROUP BY state ORDER BY state")
            .fetch_all(c)
            .await?,
    )
}

/// Uuid helper for callers that only have raw ids.
pub fn uuid(id: impl Into<Uuid>) -> Uuid {
    id.into()
}
