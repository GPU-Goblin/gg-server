//! Collective training runs, rounds and published checkpoints.

use ::time::OffsetDateTime;
use sqlx::FromRow;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::StorageError;

/// Run row.
#[derive(Clone, Debug, FromRow)]
pub struct CollectiveRunRow {
    /// Id.
    pub id: Uuid,
    /// Project.
    pub project_id: Uuid,
    /// Experiment.
    pub experiment_id: Uuid,
    /// Name.
    pub name: String,
    /// State.
    pub state: String,
    /// Config JSON.
    pub config: serde_json::Value,
    /// Rounds aggregated.
    pub step: i32,
    /// Current checkpoint JSON.
    pub checkpoint: serde_json::Value,
    /// Failure text.
    pub failure: String,
    /// Created.
    pub created_at: OffsetDateTime,
    /// Updated.
    pub updated_at: OffsetDateTime,
    /// Lab node that steps the run (ADR-0041), when one does.
    pub holder_node: Option<Uuid>,
}

/// Round row.
#[derive(Clone, Debug, FromRow)]
pub struct CollectiveRoundRow {
    /// Id.
    pub id: Uuid,
    /// Run.
    pub run_id: Uuid,
    /// Step.
    pub step: i32,
    /// Attempt.
    pub attempt: i32,
    /// Seed.
    pub round_seed: i64,
    /// Job.
    pub job_id: Uuid,
    /// State.
    pub state: String,
    /// Expected examples.
    pub expected_examples: i64,
    /// Created.
    pub created_at: OffsetDateTime,
    /// Closed.
    pub closed_at: Option<OffsetDateTime>,
}

const RUN_COLS: &str = "id, project_id, experiment_id, name, state, config, step, checkpoint, failure, created_at, updated_at, holder_node";
const ROUND_COLS: &str = "id, run_id, step, attempt, round_seed, job_id, state, expected_examples, created_at, closed_at";

/// Insert a run.
#[allow(clippy::too_many_arguments)]
pub async fn insert_run(
    c: &mut PgConnection,
    id: Uuid,
    project: Uuid,
    experiment: Uuid,
    created_by: Option<Uuid>,
    name: &str,
    config: &serde_json::Value,
    checkpoint: &serde_json::Value,
) -> Result<(), StorageError> {
    sqlx::query("INSERT INTO collective_runs (id, project_id, experiment_id, created_by, name, state, config, checkpoint) VALUES ($1, $2, $3, $4, $5, 'running', $6, $7)")
        .bind(id)
        .bind(project)
        .bind(experiment)
        .bind(created_by)
        .bind(name)
        .bind(config)
        .bind(checkpoint)
        .execute(c)
        .await?;
    Ok(())
}

/// Get a run.
pub async fn get_run(
    c: &mut PgConnection,
    id: Uuid,
) -> Result<Option<CollectiveRunRow>, StorageError> {
    Ok(
        sqlx::query_as::<_, CollectiveRunRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {RUN_COLS} FROM collective_runs WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(c)
        .await?,
    )
}

/// Lock a run.
pub async fn lock_run(
    c: &mut PgConnection,
    id: Uuid,
) -> Result<Option<CollectiveRunRow>, StorageError> {
    Ok(
        sqlx::query_as::<_, CollectiveRunRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {RUN_COLS} FROM collective_runs WHERE id = $1 FOR UPDATE"
        )))
        .bind(id)
        .fetch_optional(c)
        .await?,
    )
}

/// Update run progress.
pub async fn update_run(
    c: &mut PgConnection,
    id: Uuid,
    state: &str,
    step: i32,
    checkpoint: &serde_json::Value,
    failure: &str,
) -> Result<(), StorageError> {
    sqlx::query("UPDATE collective_runs SET state = $2, step = $3, checkpoint = $4, failure = $5, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(state)
        .bind(step)
        .bind(checkpoint)
        .bind(failure)
        .execute(c)
        .await?;
    Ok(())
}

/// Insert a round.
#[allow(clippy::too_many_arguments)]
pub async fn insert_round(
    c: &mut PgConnection,
    id: Uuid,
    run: Uuid,
    step: i32,
    attempt: i32,
    round_seed: i64,
    job: Uuid,
    expected_examples: i64,
) -> Result<(), StorageError> {
    sqlx::query("INSERT INTO collective_rounds (id, run_id, step, attempt, round_seed, job_id, state, expected_examples) VALUES ($1, $2, $3, $4, $5, $6, 'open', $7)")
        .bind(id)
        .bind(run)
        .bind(step)
        .bind(attempt)
        .bind(round_seed)
        .bind(job)
        .bind(expected_examples)
        .execute(c)
        .await?;
    Ok(())
}

/// Stage state of a staged round (P11 D2), `None` for a data-parallel round.
pub async fn stage_state(
    c: &mut PgConnection,
    round: Uuid,
) -> Result<Option<serde_json::Value>, StorageError> {
    let (v,): (Option<serde_json::Value>,) =
        sqlx::query_as("SELECT stage_state FROM collective_rounds WHERE id = $1")
            .bind(round)
            .fetch_one(c)
            .await?;
    Ok(v)
}

/// Move a staged round to its next wave: the job of that wave, and what earlier waves produced.
pub async fn set_stage_state(
    c: &mut PgConnection,
    round: Uuid,
    job: Uuid,
    state: &serde_json::Value,
) -> Result<(), StorageError> {
    sqlx::query("UPDATE collective_rounds SET job_id = $2, stage_state = $3 WHERE id = $1 AND state = 'open'")
        .bind(round)
        .bind(job)
        .bind(state)
        .execute(c)
        .await?;
    Ok(())
}

/// Open rounds, locked for the coordinator (skips rounds handled by another coordinator pass).
pub async fn lock_open_rounds(
    c: &mut PgConnection,
    limit: i64,
) -> Result<Vec<CollectiveRoundRow>, StorageError> {
    Ok(sqlx::query_as::<_, CollectiveRoundRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {ROUND_COLS} FROM collective_rounds WHERE state = 'open' ORDER BY created_at LIMIT $1 FOR UPDATE SKIP LOCKED"
    )))
    .bind(limit)
    .fetch_all(c)
    .await?)
}

/// Rounds of a run.
pub async fn list_rounds(
    c: &mut PgConnection,
    run: Uuid,
) -> Result<Vec<CollectiveRoundRow>, StorageError> {
    Ok(
        sqlx::query_as::<_, CollectiveRoundRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {ROUND_COLS} FROM collective_rounds WHERE run_id = $1 ORDER BY step, attempt"
        )))
        .bind(run)
        .fetch_all(c)
        .await?,
    )
}

/// Close a round.
pub async fn close_round(c: &mut PgConnection, id: Uuid, state: &str) -> Result<(), StorageError> {
    sqlx::query("UPDATE collective_rounds SET state = $2, closed_at = now() WHERE id = $1")
        .bind(id)
        .bind(state)
        .execute(c)
        .await?;
    Ok(())
}

/// Publish a checkpoint (unique per run/step).
pub async fn insert_checkpoint(
    c: &mut PgConnection,
    run: Uuid,
    step: i32,
    checkpoint: &serde_json::Value,
    from_round: Option<Uuid>,
) -> Result<(), StorageError> {
    sqlx::query("INSERT INTO collective_checkpoints (run_id, step, checkpoint, from_round) VALUES ($1, $2, $3, $4)")
        .bind(run)
        .bind(step)
        .bind(checkpoint)
        .bind(from_round)
        .execute(c)
        .await?;
    Ok(())
}

/// The round whose aggregation produced checkpoint `step` of a run.
pub async fn checkpoint_round(
    c: &mut PgConnection,
    run: Uuid,
    step: i32,
) -> Result<Option<CollectiveRoundRow>, StorageError> {
    Ok(
        sqlx::query_as::<_, CollectiveRoundRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {ROUND_COLS} FROM collective_rounds WHERE id = \
             (SELECT from_round FROM collective_checkpoints WHERE run_id = $1 AND step = $2)"
        )))
        .bind(run)
        .bind(step)
        .fetch_optional(c)
        .await?,
    )
}

/// Closed rounds not yet examined by the retention policy (ADR-0040), closed more than
/// `older_than_secs` ago, oldest first.
pub async fn prunable_rounds(
    c: &mut PgConnection,
    older_than_secs: i64,
    limit: i64,
) -> Result<Vec<CollectiveRoundRow>, StorageError> {
    Ok(
        sqlx::query_as::<_, CollectiveRoundRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {ROUND_COLS} FROM collective_rounds
         WHERE pruned_at IS NULL AND state IN ('aggregated', 'failed')
           AND closed_at < now() - make_interval(secs => $1)
         ORDER BY closed_at LIMIT $2"
        )))
        .bind(older_than_secs as f64)
        .bind(limit)
        .fetch_all(c)
        .await?,
    )
}

/// Rounds of a run the retention policy has not examined yet (open ones included).
pub async fn unpruned_rounds(
    c: &mut PgConnection,
    run: Uuid,
) -> Result<Vec<CollectiveRoundRow>, StorageError> {
    Ok(
        sqlx::query_as::<_, CollectiveRoundRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {ROUND_COLS} FROM collective_rounds WHERE run_id = $1 AND pruned_at IS NULL"
        )))
        .bind(run)
        .fetch_all(c)
        .await?,
    )
}

/// Record that a round's intermediate artifacts were examined.
pub async fn mark_round_pruned(c: &mut PgConnection, round: Uuid) -> Result<(), StorageError> {
    sqlx::query("UPDATE collective_rounds SET pruned_at = now() WHERE id = $1")
        .bind(round)
        .execute(c)
        .await?;
    Ok(())
}

/// Step and references of the checkpoint a round's aggregation produced.
pub async fn checkpoint_of_round(
    c: &mut PgConnection,
    round: Uuid,
) -> Result<Option<(i32, serde_json::Value)>, StorageError> {
    Ok(sqlx::query_as::<_, (i32, serde_json::Value)>(
        "SELECT step, checkpoint FROM collective_checkpoints WHERE from_round = $1",
    )
    .bind(round)
    .fetch_optional(c)
    .await?)
}

/// Checkpoints of a run at the given steps.
pub async fn checkpoints_at(
    c: &mut PgConnection,
    run: Uuid,
    steps: &[i32],
) -> Result<Vec<(i32, serde_json::Value)>, StorageError> {
    Ok(sqlx::query_as::<_, (i32, serde_json::Value)>(
        "SELECT step, checkpoint FROM collective_checkpoints WHERE run_id = $1 AND step = ANY($2)",
    )
    .bind(run)
    .bind(steps)
    .fetch_all(c)
    .await?)
}

/// Every step at which a run recorded a checkpoint.
pub async fn checkpoint_steps(c: &mut PgConnection, run: Uuid) -> Result<Vec<i32>, StorageError> {
    Ok(
        sqlx::query_scalar::<_, i32>("SELECT step FROM collective_checkpoints WHERE run_id = $1")
            .bind(run)
            .fetch_all(c)
            .await?,
    )
}

/// Configuration and current checkpoint of every run of a project, as JSON text.
pub async fn project_run_references(
    c: &mut PgConnection,
    project: Uuid,
) -> Result<Vec<String>, StorageError> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT config::text || ' ' || checkpoint::text FROM collective_runs WHERE project_id = $1",
    )
    .bind(project)
    .fetch_all(c)
    .await?)
}

/// Published checkpoints of a run.
pub async fn list_checkpoints(
    c: &mut PgConnection,
    run: Uuid,
) -> Result<Vec<(i32, serde_json::Value)>, StorageError> {
    Ok(sqlx::query_as(
        "SELECT step, checkpoint FROM collective_checkpoints WHERE run_id = $1 ORDER BY step",
    )
    .bind(run)
    .fetch_all(c)
    .await?)
}

/// Runs of a project.
pub async fn list_runs(
    c: &mut PgConnection,
    project: Uuid,
) -> Result<Vec<CollectiveRunRow>, StorageError> {
    Ok(
        sqlx::query_as::<_, CollectiveRunRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {RUN_COLS} FROM collective_runs WHERE project_id = $1 ORDER BY created_at DESC"
        )))
        .bind(project)
        .fetch_all(c)
        .await?,
    )
}

/// Make `node` the one that steps a run (ADR-0041).
pub async fn set_run_holder(
    c: &mut PgConnection,
    run: Uuid,
    node: Uuid,
) -> Result<(), StorageError> {
    sqlx::query("UPDATE collective_runs SET holder_node = $2 WHERE id = $1")
        .bind(run)
        .bind(node)
        .execute(c)
        .await?;
    Ok(())
}

/// Running runs a lab node steps.
pub async fn running_runs_of_node(
    c: &mut PgConnection,
    node: Uuid,
) -> Result<Vec<CollectiveRunRow>, StorageError> {
    Ok(
        sqlx::query_as::<_, CollectiveRunRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {RUN_COLS} FROM collective_runs WHERE holder_node = $1 AND state = 'running' ORDER BY created_at"
        )))
        .bind(node)
        .fetch_all(c)
        .await?,
    )
}

/// Whether a run `node` steps names `hash` in its configuration (its description, its datasets).
pub async fn node_run_names(
    c: &mut PgConnection,
    node: Uuid,
    hash: &gg_core::Sha256Digest,
) -> Result<bool, StorageError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM collective_runs
          WHERE holder_node = $1 AND position($2 in config::text) > 0)",
    )
    .bind(node)
    .bind(hash.to_hex())
    .fetch_one(c)
    .await?)
}

/// The open round of a run, if there is one.
pub async fn open_round_of(
    c: &mut PgConnection,
    run: Uuid,
) -> Result<Option<CollectiveRoundRow>, StorageError> {
    Ok(sqlx::query_as::<_, CollectiveRoundRow>(
        "SELECT id, run_id, step, attempt, round_seed, job_id, state, expected_examples, created_at, closed_at
           FROM collective_rounds WHERE run_id = $1 AND state = 'open' ORDER BY created_at DESC LIMIT 1",
    )
    .bind(run)
    .fetch_optional(c)
    .await?)
}

/// Record the rows a lab node cut for `step` of `run`. Returns the rows already recorded for that
/// step instead when there are some (they are never replaced).
pub async fn insert_node_rows(
    c: &mut PgConnection,
    run: Uuid,
    step: i32,
    rows: &serde_json::Value,
) -> Result<Option<serde_json::Value>, StorageError> {
    let inserted = sqlx::query(
        "INSERT INTO node_round_rows (run_id, step, rows) VALUES ($1, $2, $3)
         ON CONFLICT (run_id, step) DO NOTHING",
    )
    .bind(run)
    .bind(step)
    .bind(rows)
    .execute(&mut *c)
    .await?
    .rows_affected();
    if inserted == 1 {
        return Ok(None);
    }
    node_rows(c, run, step).await
}

/// The rows a lab node cut for `step` of `run`.
pub async fn node_rows(
    c: &mut PgConnection,
    run: Uuid,
    step: i32,
) -> Result<Option<serde_json::Value>, StorageError> {
    Ok(sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT rows FROM node_round_rows WHERE run_id = $1 AND step = $2",
    )
    .bind(run)
    .bind(step)
    .fetch_optional(c)
    .await?)
}
