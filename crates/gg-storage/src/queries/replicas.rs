//! Replicated computation (ADR-0042): the computations of one task (its "family": the first one
//! and its replicas), the contributor accounts that took part, the drawing of a replica's worker,
//! and the comparisons between computations.

use gg_core::*;
use sqlx::PgConnection;
use uuid::Uuid;

use super::jobs::{TASK_COLS, TaskRequirements};
use crate::StorageError;
use crate::models::TaskRow;

/// The twin job of `job` for rank `seq` (where its computations of that rank live), if created.
pub async fn replica_job(
    c: &mut PgConnection,
    job: Uuid,
    seq: i16,
) -> Result<Option<Uuid>, StorageError> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM jobs WHERE replica_of = $1 AND replica_seq = $2",
    )
    .bind(job)
    .bind(seq)
    .fetch_optional(c)
    .await?)
}

/// Make `twin` the twin job of `job` for rank `seq`, with the same output holder if `same_holder`.
pub async fn set_replica_job(
    c: &mut PgConnection,
    twin: Uuid,
    job: Uuid,
    seq: i16,
    same_holder: bool,
) -> Result<(), StorageError> {
    sqlx::query(
        "UPDATE jobs SET replica_of = $2, replica_seq = $3,
            output_holder = CASE WHEN $4 THEN (SELECT output_holder FROM jobs WHERE id = $2) END
          WHERE id = $1",
    )
    .bind(twin)
    .bind(job)
    .bind(seq)
    .bind(same_holder)
    .execute(c)
    .await?;
    Ok(())
}

/// Mark `task` as computation `seq` of `family`, offered to `designated` alone for `reserve_secs`.
pub async fn mark_replica(
    c: &mut PgConnection,
    task: TaskId,
    family: TaskId,
    seq: i16,
    designated: Option<WorkerId>,
    reserve_secs: i64,
) -> Result<(), StorageError> {
    sqlx::query(
        "UPDATE tasks SET replica_of = $2, replica_seq = $3, designated_worker = $4,
            designated_until = CASE WHEN $4::uuid IS NULL THEN NULL
                                    ELSE now() + make_interval(secs => $5) END
          WHERE id = $1",
    )
    .bind(task.0)
    .bind(family.0)
    .bind(seq)
    .bind(designated.map(|w| w.0))
    .bind(reserve_secs as f64)
    .execute(c)
    .await?;
    Ok(())
}

/// The computations of `family`: the first, then its replicas by `replica_seq`.
pub async fn members(c: &mut PgConnection, family: TaskId) -> Result<Vec<TaskRow>, StorageError> {
    Ok(sqlx::query_as::<_, TaskRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {TASK_COLS} FROM tasks WHERE id = $1 OR replica_of = $1 ORDER BY replica_seq"
    )))
    .bind(family.0)
    .fetch_all(c)
    .await?)
}

/// What a task requires of a worker (C1).
pub async fn requirements(
    c: &mut PgConnection,
    task: TaskId,
) -> Result<TaskRequirements, StorageError> {
    let (ram_bytes, device_bytes, profile): (i64, i64, i32) = sqlx::query_as(
        "SELECT required_ram_bytes, required_device_bytes, required_profile FROM tasks WHERE id = $1",
    )
    .bind(task.0)
    .fetch_one(c)
    .await?;
    Ok(TaskRequirements {
        ram_bytes,
        device_bytes,
        profile,
    })
}

/// The account of a worker: its contributor account, or the worker itself when it has none.
pub async fn account_of(c: &mut PgConnection, worker: WorkerId) -> Result<Uuid, StorageError> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT COALESCE(contributor_id, id) FROM workers WHERE id = $1",
    )
    .bind(worker.0)
    .fetch_one(c)
    .await?)
}

/// The accounts that computed (or are computing) any computation of `family`.
pub async fn family_accounts(
    c: &mut PgConnection,
    family: TaskId,
) -> Result<Vec<Uuid>, StorageError> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT DISTINCT COALESCE(w.contributor_id, w.id)
           FROM tasks t JOIN attempts a ON a.task_id = t.id JOIN workers w ON w.id = a.worker_id
          WHERE t.id = $1 OR t.replica_of = $1 OR t.compare_family = $1",
    )
    .bind(family.0)
    .fetch_all(c)
    .await?)
}

/// What a computation needs of the worker drawn for it.
pub struct Need<'a> {
    /// Compute kind.
    pub kind: &'a str,
    /// Memory and profile.
    pub requirements: TaskRequirements,
    /// Its job's outputs go to a lab node: the worker must serve peers.
    pub serves_peers: bool,
}

/// Workers that could take a computation needing `need`, seen within `within_secs`, of none of
/// the `excluded` accounts, and whose account is not suspended.
pub async fn candidates(
    c: &mut PgConnection,
    need: &Need<'_>,
    excluded: &[Uuid],
    within_secs: i64,
) -> Result<Vec<WorkerId>, StorageError> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT w.id FROM workers w LEFT JOIN contributors k ON k.id = w.contributor_id
          WHERE w.authorized AND w.announced_at > now() - make_interval(secs => $1)
            AND $2 = ANY(w.announced_kinds)
            AND (w.announced_ram_bytes IS NULL OR w.announced_ram_bytes >= $3)
            AND (w.announced_device_bytes IS NULL OR w.announced_device_bytes >= $4)
            AND COALESCE(w.announced_profile, 1) >= $5
            AND (NOT $6 OR w.announced_serves_peers)
            AND k.suspended_at IS NULL
            AND NOT (COALESCE(w.contributor_id, w.id) = ANY($7))",
    )
    .bind(within_secs as f64)
    .bind(need.kind)
    .bind(need.requirements.ram_bytes)
    .bind(need.requirements.device_bytes)
    .bind(need.requirements.profile)
    .bind(need.serves_peers)
    .bind(excluded)
    .fetch_all(c)
    .await?
    .into_iter()
    .map(WorkerId)
    .collect())
}

/// Families with a computation submitted and not decided yet, oldest first.
pub async fn families_to_compare(
    c: &mut PgConnection,
    limit: i64,
) -> Result<Vec<TaskId>, StorageError> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT f FROM (
            SELECT COALESCE(t.replica_of, t.compare_family, t.id) AS f, min(t.updated_at) AS since
              FROM tasks t
             WHERE t.state = 'submitted'
               AND (t.replica_of IS NOT NULL OR t.compare_family IS NOT NULL
                    OR EXISTS (SELECT 1 FROM tasks r WHERE r.replica_of = t.id))
             GROUP BY 1) x
          ORDER BY since LIMIT $1",
    )
    .bind(limit)
    .fetch_all(c)
    .await?
    .into_iter()
    .map(TaskId)
    .collect())
}

/// A recorded comparison.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ComparisonRow {
    /// Id.
    pub id: Uuid,
    /// The first computation's task.
    pub family: Uuid,
    /// One attempt (the smaller id).
    pub attempt_a: Uuid,
    /// The other.
    pub attempt_b: Uuid,
    /// `server`, or `node:<id>`.
    pub comparer: String,
    /// Outcome text.
    pub outcome: String,
    /// Hash of the signed comparison.
    pub comparison_hash: Vec<u8>,
    /// The server's replay of a lab node's comparison, when replayed.
    pub replayed_outcome: Option<String>,
    /// Hash of the server's signed replay.
    pub replayed_hash: Option<Vec<u8>>,
}

impl ComparisonRow {
    /// Parsed outcome.
    pub fn outcome(&self) -> Result<VerificationOutcome, StorageError> {
        self.outcome
            .parse()
            .map_err(|_| StorageError::Corrupt(format!("comparison outcome {}", self.outcome)))
    }
    /// Hash of the signed comparison that counts: the server's replay when there is one.
    pub fn digest(&self) -> Result<Sha256Digest, StorageError> {
        let raw = self.replayed_hash.as_ref().unwrap_or(&self.comparison_hash);
        let b: [u8; 32] = raw
            .as_slice()
            .try_into()
            .map_err(|_| StorageError::Corrupt("comparison hash".into()))?;
        Ok(Sha256Digest(b))
    }
    /// A lab node compared them.
    pub fn by_node(&self) -> bool {
        self.comparer.starts_with("node:")
    }
    /// The outcome that counts: the server's replay when there is one; a node's finding of a
    /// difference not replayed yet does not count (`None`).
    pub fn counting(&self) -> Result<Option<VerificationOutcome>, StorageError> {
        if let Some(r) = &self.replayed_outcome {
            return r
                .parse()
                .map(Some)
                .map_err(|_| StorageError::Corrupt(format!("replayed outcome {r}")));
        }
        let o = self.outcome()?;
        Ok((!self.by_node() || o == VerificationOutcome::Accepted).then_some(o))
    }
    /// Whether it compared `x` and `y` (in either order).
    pub fn of(&self, x: Uuid, y: Uuid) -> bool {
        (self.attempt_a == x && self.attempt_b == y) || (self.attempt_a == y && self.attempt_b == x)
    }
}

/// A new comparison.
pub struct NewComparison<'a> {
    /// The first computation's task.
    pub family: TaskId,
    /// One attempt.
    pub attempt_a: AttemptId,
    /// The other.
    pub attempt_b: AttemptId,
    /// `server`, or `node:<id>`.
    pub comparer: &'a str,
    /// Outcome.
    pub outcome: VerificationOutcome,
    /// Hash of the signed comparison.
    pub hash: &'a Sha256Digest,
    /// Its canonical bytes.
    pub signed_cbor: &'a [u8],
}

/// Record a comparison; `false` when that pair was compared already (in either order).
pub async fn insert_comparison(
    c: &mut PgConnection,
    n: &NewComparison<'_>,
) -> Result<bool, StorageError> {
    let (a, b) = if n.attempt_a.0 <= n.attempt_b.0 {
        (n.attempt_a.0, n.attempt_b.0)
    } else {
        (n.attempt_b.0, n.attempt_a.0)
    };
    let r = sqlx::query(
        "INSERT INTO comparisons (id, family, attempt_a, attempt_b, comparer, outcome, comparison_hash, signed_cbor)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (attempt_a, attempt_b) DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .bind(n.family.0)
    .bind(a)
    .bind(b)
    .bind(n.comparer)
    .bind(n.outcome.as_str())
    .bind(n.hash.as_bytes().as_slice())
    .bind(n.signed_cbor)
    .execute(c)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// The comparisons recorded for `family`.
pub async fn comparisons(
    c: &mut PgConnection,
    family: TaskId,
) -> Result<Vec<ComparisonRow>, StorageError> {
    Ok(sqlx::query_as::<_, ComparisonRow>(
        "SELECT id, family, attempt_a, attempt_b, comparer, outcome, comparison_hash,
                replayed_outcome, replayed_hash
           FROM comparisons WHERE family = $1 ORDER BY created_at",
    )
    .bind(family.0)
    .fetch_all(c)
    .await?)
}

/// Record the check of a lab node's comparison: the outcome that counts, the hash of the signed
/// submission that established it, and by whom.
pub async fn set_replay(
    c: &mut PgConnection,
    id: Uuid,
    outcome: VerificationOutcome,
    hash: &Sha256Digest,
    by: &str,
) -> Result<(), StorageError> {
    sqlx::query(
        "UPDATE comparisons SET replayed_outcome = $2, replayed_hash = $3, replayed_by = $4 WHERE id = $1",
    )
    .bind(id)
    .bind(outcome.as_str())
    .bind(hash.as_bytes().as_slice())
    .bind(by)
    .execute(c)
    .await?;
    Ok(())
}

/// Families of the jobs `node` holds with a computation submitted and not decided yet.
pub async fn node_families(
    c: &mut PgConnection,
    node: NodeId,
    limit: i64,
) -> Result<Vec<TaskId>, StorageError> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT DISTINCT COALESCE(t.replica_of, t.id) FROM tasks t JOIN jobs j ON j.id = t.job_id
          WHERE t.state = 'submitted' AND j.output_holder = $1
            AND (t.replica_of IS NOT NULL OR EXISTS (SELECT 1 FROM tasks r WHERE r.replica_of = t.id))
          LIMIT $2",
    )
    .bind(node.0)
    .bind(limit)
    .fetch_all(c)
    .await?
    .into_iter()
    .map(TaskId)
    .collect())
}

/// Suspend a lab node pending a human (a misreported comparison).
pub async fn suspend_node(
    c: &mut PgConnection,
    node: NodeId,
    reason: &str,
) -> Result<(), StorageError> {
    sqlx::query(
        "UPDATE lab_nodes SET suspended_at = COALESCE(suspended_at, now()), suspended_reason = $2 WHERE id = $1",
    )
    .bind(node.0)
    .bind(reason)
    .execute(c)
    .await?;
    Ok(())
}

/// Whether a lab node is suspended.
pub async fn node_suspended(c: &mut PgConnection, node: NodeId) -> Result<bool, StorageError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT suspended_at IS NOT NULL FROM lab_nodes WHERE id = $1",
    )
    .bind(node.0)
    .fetch_optional(c)
    .await?
    .unwrap_or(true))
}

/// Mark `task` as a comparer of attempts `a` and `b` of `family`, offered to `designated` alone
/// for `reserve_secs`.
pub async fn mark_comparer(
    c: &mut PgConnection,
    task: TaskId,
    family: TaskId,
    (a, b): (AttemptId, AttemptId),
    designated: Option<WorkerId>,
    reserve_secs: i64,
) -> Result<(), StorageError> {
    sqlx::query(
        "UPDATE tasks SET compare_family = $2, compare_a = $3, compare_b = $4, designated_worker = $5,
            designated_until = CASE WHEN $5::uuid IS NULL THEN NULL
                                    ELSE now() + make_interval(secs => $6) END
          WHERE id = $1",
    )
    .bind(task.0)
    .bind(family.0)
    .bind(a.0)
    .bind(b.0)
    .bind(designated.map(|w| w.0))
    .bind(reserve_secs as f64)
    .execute(c)
    .await?;
    Ok(())
}

/// The comparer tasks of attempts `a` and `b` (in either order), oldest first.
pub async fn comparers(
    c: &mut PgConnection,
    a: Uuid,
    b: Uuid,
) -> Result<Vec<TaskRow>, StorageError> {
    Ok(sqlx::query_as::<_, TaskRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {TASK_COLS} FROM tasks
          WHERE (compare_a = $1 AND compare_b = $2) OR (compare_a = $2 AND compare_b = $1)
          ORDER BY created_at"
    )))
    .bind(a)
    .bind(b)
    .fetch_all(c)
    .await?)
}

/// The next free index in `job`.
pub async fn next_index(c: &mut PgConnection, job: Uuid) -> Result<i32, StorageError> {
    Ok(sqlx::query_scalar::<_, Option<i32>>(
        "SELECT max(index_in_job) + 1 FROM tasks WHERE job_id = $1",
    )
    .bind(job)
    .fetch_one(c)
    .await?
    .unwrap_or(0))
}

/// Mark `family` as spot-checked: it waits for its third computation before any decision.
pub async fn set_spot_check(c: &mut PgConnection, family: TaskId) -> Result<(), StorageError> {
    sqlx::query("UPDATE tasks SET spot_check = true WHERE id = $1")
        .bind(family.0)
        .execute(c)
        .await?;
    Ok(())
}

/// Whether `family` is spot-checked.
pub async fn spot_checked(c: &mut PgConnection, family: TaskId) -> Result<bool, StorageError> {
    Ok(
        sqlx::query_scalar::<_, bool>("SELECT spot_check FROM tasks WHERE id = $1")
            .bind(family.0)
            .fetch_optional(c)
            .await?
            .unwrap_or(false),
    )
}

/// How many computations of `account`'s workers were accepted.
pub async fn accepted_of_account(c: &mut PgConnection, account: Uuid) -> Result<i64, StorageError> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM attempts a JOIN workers w ON w.id = a.worker_id
          WHERE a.state = 'accepted' AND COALESCE(w.contributor_id, w.id) = $1",
    )
    .bind(account)
    .fetch_one(c)
    .await?)
}

/// The twin jobs of `jobs` (replicas, third computations, comparers).
pub async fn twin_jobs(c: &mut PgConnection, jobs: &[Uuid]) -> Result<Vec<Uuid>, StorageError> {
    Ok(
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM jobs WHERE replica_of = ANY($1)")
            .bind(jobs)
            .fetch_all(c)
            .await?,
    )
}
