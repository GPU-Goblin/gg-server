//! Outputs handed on before their durable copy exists (ADR-0026).

use gg_core::Sha256Digest;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::StorageError;

/// A declared output that is not stored yet.
#[derive(Clone, Debug)]
pub struct PendingRow {
    /// Attempt that produced it.
    pub attempt_id: Uuid,
    /// Producing worker.
    pub worker_id: Uuid,
    /// Project.
    pub project_id: Uuid,
    /// Declared size.
    pub size: u64,
    /// Chunk size.
    pub chunk_size: u32,
    /// Chunk digests, in order.
    pub digests: Vec<Sha256Digest>,
}

/// Data of a declaration.
pub struct NewPending<'a> {
    /// Hash of the output.
    pub hash: &'a Sha256Digest,
    /// Attempt.
    pub attempt_id: Uuid,
    /// Worker.
    pub worker_id: Uuid,
    /// Project.
    pub project_id: Uuid,
    /// Size.
    pub size: u64,
    /// Chunk size.
    pub chunk_size: u32,
    /// Chunk digests.
    pub digests: &'a [Sha256Digest],
    /// Seconds the producer is given to upload the durable copy.
    pub deadline_secs: i64,
}

/// Records a declaration; replaying the same declaration changes nothing.
pub async fn declare(c: &mut PgConnection, p: &NewPending<'_>) -> Result<(), StorageError> {
    let mut digests = Vec::with_capacity(p.digests.len() * 32);
    for d in p.digests {
        digests.extend_from_slice(d.as_bytes());
    }
    sqlx::query(
        "INSERT INTO pending_outputs (hash, attempt_id, worker_id, project_id, size, chunk_size,
            chunk_count, digests, deadline)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now() + make_interval(secs => $9))
         ON CONFLICT (hash, attempt_id) DO NOTHING",
    )
    .bind(p.hash.as_bytes().as_slice())
    .bind(p.attempt_id)
    .bind(p.worker_id)
    .bind(p.project_id)
    .bind(p.size as i64)
    .bind(p.chunk_size as i32)
    .bind(p.digests.len() as i32)
    .bind(digests)
    .bind(p.deadline_secs as f64)
    .execute(c)
    .await?;
    Ok(())
}

type Row = (Uuid, Uuid, Uuid, i64, i32, Vec<u8>);

fn to_row(r: Row) -> Result<PendingRow, StorageError> {
    let digests =
        r.5.chunks(32)
            .map(|c| {
                <[u8; 32]>::try_from(c)
                    .map(Sha256Digest)
                    .map_err(|_| StorageError::Corrupt("pending digests".into()))
            })
            .collect::<Result<_, _>>()?;
    Ok(PendingRow {
        attempt_id: r.0,
        worker_id: r.1,
        project_id: r.2,
        size: r.3 as u64,
        chunk_size: r.4 as u32,
        digests,
    })
}

/// The oldest declaration of a hash, if there is one.
pub async fn get(
    c: &mut PgConnection,
    hash: &Sha256Digest,
) -> Result<Option<PendingRow>, StorageError> {
    let r: Option<Row> = sqlx::query_as(
        "SELECT attempt_id, worker_id, project_id, size, chunk_size, digests FROM pending_outputs
          WHERE hash = $1 ORDER BY created_at LIMIT 1",
    )
    .bind(hash.as_bytes().as_slice())
    .fetch_optional(c)
    .await?;
    r.map(to_row).transpose()
}

/// Whether `worker` declared `hash` (several attempts may produce the same bytes, such as a
/// constant activation of every micro-batch).
pub async fn declared_by(
    c: &mut PgConnection,
    hash: &Sha256Digest,
    worker: Uuid,
) -> Result<bool, StorageError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM pending_outputs WHERE hash = $1 AND worker_id = $2)",
    )
    .bind(hash.as_bytes().as_slice())
    .bind(worker)
    .fetch_one(c)
    .await?)
}

/// The declaration of a hash by a given attempt.
pub async fn get_for_attempt(
    c: &mut PgConnection,
    hash: &Sha256Digest,
    attempt: Uuid,
) -> Result<Option<PendingRow>, StorageError> {
    let r: Option<Row> = sqlx::query_as(
        "SELECT attempt_id, worker_id, project_id, size, chunk_size, digests FROM pending_outputs
          WHERE hash = $1 AND attempt_id = $2",
    )
    .bind(hash.as_bytes().as_slice())
    .bind(attempt)
    .fetch_optional(c)
    .await?;
    r.map(to_row).transpose()
}

/// The durable copy has arrived: the hash is no longer pending, for any attempt.
pub async fn resolve(c: &mut PgConnection, hash: &Sha256Digest) -> Result<u64, StorageError> {
    Ok(sqlx::query("DELETE FROM pending_outputs WHERE hash = $1")
        .bind(hash.as_bytes().as_slice())
        .execute(c)
        .await?
        .rows_affected())
}

/// Among `hashes`, those still waiting for their durable copy.
pub async fn still_pending(
    c: &mut PgConnection,
    hashes: &[Sha256Digest],
) -> Result<Vec<Sha256Digest>, StorageError> {
    if hashes.is_empty() {
        return Ok(Vec::new());
    }
    let bytes: Vec<Vec<u8>> = hashes.iter().map(|h| h.as_bytes().to_vec()).collect();
    let rows: Vec<(Vec<u8>,)> =
        sqlx::query_as("SELECT DISTINCT hash FROM pending_outputs WHERE hash = ANY($1)")
            .bind(bytes)
            .fetch_all(c)
            .await?;
    rows.into_iter()
        .map(|(h,)| {
            <[u8; 32]>::try_from(h.as_slice())
                .map(Sha256Digest)
                .map_err(|_| StorageError::Corrupt("pending hash".into()))
        })
        .collect()
}

/// Among `attempts`, those with a declared output past its deadline without a durable copy.
pub async fn overdue(c: &mut PgConnection, attempts: &[Uuid]) -> Result<Vec<Uuid>, StorageError> {
    if attempts.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT attempt_id FROM pending_outputs WHERE attempt_id = ANY($1) AND deadline < now()",
    )
    .bind(attempts)
    .fetch_all(c)
    .await?)
}

/// An output a lab node is to pull.
#[derive(Clone, Debug)]
pub struct ToPull {
    /// Hash.
    pub hash: Sha256Digest,
    /// Declared size.
    pub size: u64,
    /// Project.
    pub project_id: Uuid,
    /// Producing attempt.
    pub attempt_id: Uuid,
}

/// Outputs declared for the jobs `node` holds, not pulled yet, oldest first.
pub async fn to_pull(
    c: &mut PgConnection,
    node: Uuid,
    limit: i64,
) -> Result<Vec<ToPull>, StorageError> {
    let rows: Vec<(Vec<u8>, i64, Uuid, Uuid)> = sqlx::query_as(
        "SELECT p.hash, p.size, p.project_id, p.attempt_id FROM pending_outputs p
           JOIN attempts a ON a.id = p.attempt_id
           JOIN tasks t ON t.id = a.task_id
           JOIN jobs j ON j.id = t.job_id
          WHERE j.output_holder = $1 AND p.deadline > now() AND a.state IN ('active', 'submitted')
          ORDER BY p.created_at LIMIT $2",
    )
    .bind(node)
    .bind(limit)
    .fetch_all(c)
    .await?;
    rows.into_iter()
        .map(|(h, size, project_id, attempt_id)| {
            Ok(ToPull {
                hash: <[u8; 32]>::try_from(h.as_slice())
                    .map(Sha256Digest)
                    .map_err(|_| StorageError::Corrupt("pending hash".into()))?,
                size: size as u64,
                project_id,
                attempt_id,
            })
        })
        .collect()
}

/// Whether `hash` is an output declared for a job `node` holds, not pulled yet.
pub async fn is_for_node(
    c: &mut PgConnection,
    hash: &Sha256Digest,
    node: Uuid,
) -> Result<bool, StorageError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM pending_outputs p
           JOIN attempts a ON a.id = p.attempt_id
           JOIN tasks t ON t.id = a.task_id
           JOIN jobs j ON j.id = t.job_id
          WHERE p.hash = $1 AND j.output_holder = $2)",
    )
    .bind(hash.as_bytes().as_slice())
    .bind(node)
    .fetch_one(c)
    .await?)
}

/// Submitted attempts of jobs held by a lab node with an output the node did not pull before its
/// deadline: their tasks are sent again.
pub async fn unpulled_overdue(c: &mut PgConnection, limit: i64) -> Result<Vec<Uuid>, StorageError> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT p.attempt_id FROM pending_outputs p
           JOIN attempts a ON a.id = p.attempt_id
           JOIN tasks t ON t.id = a.task_id
           JOIN jobs j ON j.id = t.job_id
          WHERE j.output_holder IS NOT NULL AND p.deadline < now() AND a.state = 'submitted'
          LIMIT $1",
    )
    .bind(limit)
    .fetch_all(c)
    .await?)
}

/// Whether `hash` is a declared output of a job a lab node holds: the node, not the server, will
/// hold it.
pub async fn bound_for_a_node(
    c: &mut PgConnection,
    hash: &Sha256Digest,
) -> Result<bool, StorageError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM pending_outputs p
           JOIN attempts a ON a.id = p.attempt_id
           JOIN tasks t ON t.id = a.task_id
           JOIN jobs j ON j.id = t.job_id
          WHERE p.hash = $1 AND j.output_holder IS NOT NULL)",
    )
    .bind(hash.as_bytes().as_slice())
    .fetch_one(c)
    .await?)
}

/// Submitted attempts that declared `hash` for a job `node` holds.
pub async fn submitted_attempts_for_node(
    c: &mut PgConnection,
    hash: &Sha256Digest,
    node: Uuid,
) -> Result<Vec<Uuid>, StorageError> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT p.attempt_id FROM pending_outputs p
           JOIN attempts a ON a.id = p.attempt_id
           JOIN tasks t ON t.id = a.task_id
           JOIN jobs j ON j.id = t.job_id
          WHERE p.hash = $1 AND j.output_holder = $2 AND a.state = 'submitted'",
    )
    .bind(hash.as_bytes().as_slice())
    .bind(node)
    .fetch_all(c)
    .await?)
}

/// Forget the declarations of an attempt whose outputs its lab node never got, and mark it so that
/// its worker is not given the same task again.
pub async fn drop_attempt(c: &mut PgConnection, attempt: Uuid) -> Result<u64, StorageError> {
    sqlx::query("UPDATE attempts SET outputs_not_pulled = true WHERE id = $1")
        .bind(attempt)
        .execute(&mut *c)
        .await?;
    Ok(
        sqlx::query("DELETE FROM pending_outputs WHERE attempt_id = $1")
            .bind(attempt)
            .execute(c)
            .await?
            .rows_affected(),
    )
}
