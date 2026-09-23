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
