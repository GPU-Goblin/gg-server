//! Chunk digests and peer announcements for peer-assisted distribution (ADR-0012).

use gg_core::*;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::StorageError;

/// Stored chunk digests of an artifact.
#[derive(Clone, Debug)]
pub struct ChunkRow {
    /// Total size.
    pub size: u64,
    /// Chunk size (bytes).
    pub chunk_size: u32,
    /// Digests in order.
    pub digests: Vec<Sha256Digest>,
}

/// Read the chunk digests of an artifact, if they were computed.
pub async fn get_chunks(
    c: &mut PgConnection,
    hash: &Sha256Digest,
) -> Result<Option<ChunkRow>, StorageError> {
    let row: Option<(i64, i32, Vec<u8>)> =
        sqlx::query_as("SELECT size, chunk_size, digests FROM artifact_chunks WHERE hash = $1")
            .bind(hash.as_bytes().as_slice())
            .fetch_optional(c)
            .await?;
    let Some((size, chunk_size, digests)) = row else {
        return Ok(None);
    };
    if digests.len() % 32 != 0 {
        return Err(StorageError::Corrupt(
            "chunk digests not a multiple of 32".into(),
        ));
    }
    let digests = digests
        .chunks_exact(32)
        .map(|c| {
            let mut d = [0u8; 32];
            d.copy_from_slice(c);
            Sha256Digest(d)
        })
        .collect();
    Ok(Some(ChunkRow {
        size: size as u64,
        chunk_size: chunk_size as u32,
        digests,
    }))
}

/// Store the chunk digests of an artifact (idempotent: identical bytes give identical digests).
pub async fn insert_chunks(
    c: &mut PgConnection,
    hash: &Sha256Digest,
    size: u64,
    chunk_size: u32,
    digests: &[Sha256Digest],
) -> Result<(), StorageError> {
    let flat: Vec<u8> = digests.iter().flat_map(|d| d.as_bytes().to_vec()).collect();
    sqlx::query(
        "INSERT INTO artifact_chunks (hash, size, chunk_size, chunk_count, digests)
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT (hash) DO NOTHING",
    )
    .bind(hash.as_bytes().as_slice())
    .bind(size as i64)
    .bind(chunk_size as i32)
    .bind(digests.len() as i32)
    .bind(flat)
    .execute(c)
    .await?;
    Ok(())
}

/// Record (or refresh) that `worker` holds `hash` at `addr`, reachable over TLS with the transport
/// key `tls_fingerprint`, until `ttl_secs` from now.
pub async fn announce_peer(
    c: &mut PgConnection,
    hash: &Sha256Digest,
    worker: WorkerId,
    addr: &str,
    tls_fingerprint: &Sha256Digest,
    ttl_secs: i64,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO artifact_peers (hash, worker_id, addr, tls_fingerprint, expires_at)
         VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5))
         ON CONFLICT (hash, worker_id)
         DO UPDATE SET addr = EXCLUDED.addr, tls_fingerprint = EXCLUDED.tls_fingerprint,
                       announced_at = now(), expires_at = EXCLUDED.expires_at",
    )
    .bind(hash.as_bytes().as_slice())
    .bind(worker.0)
    .bind(addr)
    .bind(tls_fingerprint.as_bytes().as_slice())
    .bind(ttl_secs as f64)
    .execute(c)
    .await?;
    Ok(())
}

/// A live holder of an artifact: a worker, or a lab node (ADR-0041).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerRow {
    /// Worker holding it, when a worker does.
    pub worker_id: Option<WorkerId>,
    /// Lab node holding it, when a node does.
    pub node_id: Option<NodeId>,
    /// `ip:port`.
    pub addr: String,
    /// SHA-256 of the holder's TLS transport key.
    pub tls_fingerprint: Sha256Digest,
}

/// Live holders of `hash`, excluding the worker `exclude` (the reader itself): the lab node first when one holds it
/// (the source nobody else may have yet), then workers, most recent first.
pub async fn list_peers(
    c: &mut PgConnection,
    hash: &Sha256Digest,
    exclude: Option<WorkerId>,
    limit: i64,
) -> Result<Vec<PeerRow>, StorageError> {
    type Row = (Option<Uuid>, Option<Uuid>, String, Vec<u8>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT worker_id, node_id, addr, tls_fingerprint FROM (
             SELECT NULL::uuid AS worker_id, node_id, addr, tls_fingerprint, announced_at, 0 AS rank
             FROM node_peers WHERE hash = $1 AND expires_at > now()
             UNION ALL
             SELECT worker_id, NULL::uuid, addr, tls_fingerprint, announced_at, 1
             FROM artifact_peers WHERE hash = $1 AND ($2::uuid IS NULL OR worker_id <> $2)
               AND expires_at > now()
         ) h ORDER BY rank, announced_at DESC LIMIT $3",
    )
    .bind(hash.as_bytes().as_slice())
    .bind(exclude.map(|w| w.0))
    .bind(limit)
    .fetch_all(c)
    .await?;
    rows.into_iter()
        .map(|(w, n, addr, f)| {
            let bytes: [u8; 32] = f
                .try_into()
                .map_err(|_| StorageError::Corrupt("tls fingerprint is not 32 bytes".into()))?;
            Ok(PeerRow {
                worker_id: w.map(WorkerId),
                node_id: n.map(NodeId),
                addr,
                tls_fingerprint: Sha256Digest(bytes),
            })
        })
        .collect()
}

/// Record (or refresh) that the lab node `node` holds `hash` at `addr` until `ttl_secs` from now.
pub async fn announce_node(
    c: &mut PgConnection,
    hash: &Sha256Digest,
    node: NodeId,
    addr: &str,
    tls_fingerprint: &Sha256Digest,
    ttl_secs: i64,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO node_peers (hash, node_id, addr, tls_fingerprint, expires_at)
         VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5))
         ON CONFLICT (hash, node_id)
         DO UPDATE SET addr = EXCLUDED.addr, tls_fingerprint = EXCLUDED.tls_fingerprint,
                       announced_at = now(), expires_at = EXCLUDED.expires_at",
    )
    .bind(hash.as_bytes().as_slice())
    .bind(node.0)
    .bind(addr)
    .bind(tls_fingerprint.as_bytes().as_slice())
    .bind(ttl_secs as f64)
    .execute(c)
    .await?;
    Ok(())
}

/// Drop expired announcements (called by the reaper). Returns how many were removed.
pub async fn delete_expired_peers(c: &mut PgConnection) -> Result<u64, StorageError> {
    let nodes = sqlx::query("DELETE FROM node_peers WHERE expires_at <= now()")
        .execute(&mut *c)
        .await?
        .rows_affected();
    Ok(
        sqlx::query("DELETE FROM artifact_peers WHERE expires_at <= now()")
            .execute(c)
            .await?
            .rows_affected()
            + nodes,
    )
}

/// Drop every announcement of a worker (deauthorization, shutdown).
pub async fn delete_peers_of_worker(
    c: &mut PgConnection,
    worker: WorkerId,
) -> Result<u64, StorageError> {
    Ok(
        sqlx::query("DELETE FROM artifact_peers WHERE worker_id = $1")
            .bind(worker.0)
            .execute(c)
            .await?
            .rows_affected(),
    )
}

/// Whether `node` has a live announcement for `hash`: readers can reach it directly.
pub async fn node_announced(
    c: &mut PgConnection,
    hash: &Sha256Digest,
    node: NodeId,
) -> Result<bool, StorageError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM node_peers WHERE hash = $1 AND node_id = $2 AND expires_at > now())",
    )
    .bind(hash.as_bytes().as_slice())
    .bind(node.0)
    .fetch_one(c)
    .await?)
}
