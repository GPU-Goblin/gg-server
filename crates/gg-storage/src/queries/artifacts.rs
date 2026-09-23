//! Artifact metadata. Rows are inserted only after the bytes are durably stored and verified.

use gg_core::*;
use sqlx::PgConnection;

use crate::StorageError;
use crate::models::ArtifactRow;

/// Register an artifact (idempotent for the same hash) and grant `project_id` read access to it
/// even when another project registered the same bytes first (content addressing).
#[allow(clippy::too_many_arguments)]
pub async fn insert_artifact(
    c: &mut PgConnection,
    hash: &Sha256Digest,
    size: u64,
    media_type: &str,
    schema: &str,
    visibility: &str,
    license: &str,
    project_id: Option<ProjectId>,
    uploaded_by: &str,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO artifacts (hash, size, media_type, schema, visibility, license, project_id, uploaded_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (hash) DO UPDATE SET expired_at = NULL WHERE artifacts.expired_at IS NOT NULL",
    )
    .bind(hash.as_bytes().as_slice())
    .bind(size as i64)
    .bind(media_type)
    .bind(schema)
    .bind(visibility)
    .bind(license)
    .bind(project_id.map(|p| p.0))
    .bind(uploaded_by)
    .execute(&mut *c)
    .await?;
    if let Some(p) = project_id {
        sqlx::query(
            "INSERT INTO artifact_project_access (hash, project_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(hash.as_bytes().as_slice())
        .bind(p.0)
        .execute(c)
        .await?;
    }
    Ok(())
}

/// Who uploaded an artifact first (`worker:<id>`, `researcher:<id>`…), if it exists.
pub async fn uploaded_by(
    c: &mut PgConnection,
    hash: &Sha256Digest,
) -> Result<Option<String>, StorageError> {
    Ok(
        sqlx::query_scalar::<_, String>("SELECT uploaded_by FROM artifacts WHERE hash = $1")
            .bind(hash.as_bytes().as_slice())
            .fetch_optional(c)
            .await?,
    )
}

/// Get artifact metadata.
pub async fn get_artifact(
    c: &mut PgConnection,
    hash: &Sha256Digest,
) -> Result<Option<ArtifactRow>, StorageError> {
    Ok(sqlx::query_as::<_, ArtifactRow>(
        "SELECT hash, size, media_type, schema, visibility, license, project_id, created_at FROM artifacts WHERE hash = $1",
    )
    .bind(hash.as_bytes().as_slice())
    .fetch_optional(c)
    .await?)
}

/// Whether a project may read an artifact: public, system artifact (no owner), or granted to the
/// project (upload or materialization by the coordinator), whoever registered the hash first.
pub async fn artifact_readable_by_project(
    c: &mut PgConnection,
    hash: &Sha256Digest,
    project: ProjectId,
) -> Result<bool, StorageError> {
    let r: Option<(bool,)> = sqlx::query_as(
        "SELECT (a.visibility = 'public' OR a.project_id IS NULL OR a.project_id = $2
                 OR EXISTS (SELECT 1 FROM artifact_project_access g WHERE g.hash = a.hash AND g.project_id = $2))
         FROM artifacts a WHERE a.hash = $1",
    )
    .bind(hash.as_bytes().as_slice())
    .bind(project.0)
    .fetch_optional(c)
    .await?;
    Ok(r.map(|(b,)| b).unwrap_or(false))
}

/// Mark an artifact withdrawn. Returns false if it is unknown or already withdrawn.
pub async fn mark_withdrawn(
    c: &mut PgConnection,
    hash: &Sha256Digest,
    reason: &str,
) -> Result<bool, StorageError> {
    let r = sqlx::query(
        "UPDATE artifacts SET withdrawn_at = now(), withdrawn_reason = $2
         WHERE hash = $1 AND withdrawn_at IS NULL",
    )
    .bind(hash.as_bytes().as_slice())
    .bind(reason)
    .execute(c)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Whether the retention policy may remove this artifact (ADR-0040): something a worker uploaded or
/// the coordinator produced, neither withdrawn nor already expired. Researchers' uploads, releases
/// and anything else are never removed by it.
pub async fn retention_may_expire(
    c: &mut PgConnection,
    hash: &Sha256Digest,
) -> Result<bool, StorageError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT (uploaded_by LIKE 'worker:%' OR uploaded_by LIKE 'coordinator:%')
                AND withdrawn_at IS NULL AND expired_at IS NULL
         FROM artifacts WHERE hash = $1",
    )
    .bind(hash.as_bytes().as_slice())
    .fetch_optional(c)
    .await?
    .unwrap_or(false))
}

/// Record an artifact as removed by the retention policy, and forget its peers and chunks.
/// Returns whether it was live.
pub async fn expire(c: &mut PgConnection, hash: &Sha256Digest) -> Result<bool, StorageError> {
    let r = sqlx::query(
        "UPDATE artifacts SET expired_at = now() WHERE hash = $1 AND expired_at IS NULL",
    )
    .bind(hash.as_bytes().as_slice())
    .execute(&mut *c)
    .await?;
    for table in ["artifact_peers", "artifact_chunks"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE hash = $1"
        )))
        .bind(hash.as_bytes().as_slice())
        .execute(&mut *c)
        .await?;
    }
    Ok(r.rows_affected() == 1)
}

/// Whether an artifact was removed by the retention policy.
pub async fn is_expired(c: &mut PgConnection, hash: &Sha256Digest) -> Result<bool, StorageError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT expired_at IS NOT NULL FROM artifacts WHERE hash = $1",
    )
    .bind(hash.as_bytes().as_slice())
    .fetch_optional(c)
    .await?
    .unwrap_or(false))
}

/// Whether a task not yet decided names `hash` in its spec (a final guard before removing it).
pub async fn named_by_unfinished_task(
    c: &mut PgConnection,
    hash: &Sha256Digest,
) -> Result<bool, StorageError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM tasks
         WHERE state IN ('queued', 'assigned', 'submitted') AND position($1::bytea in spec_cbor) > 0)",
    )
    .bind(hash.as_bytes().as_slice())
    .fetch_one(c)
    .await?)
}

/// Whether an artifact was withdrawn.
pub async fn is_withdrawn(c: &mut PgConnection, hash: &Sha256Digest) -> Result<bool, StorageError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT withdrawn_at IS NOT NULL FROM artifacts WHERE hash = $1",
    )
    .bind(hash.as_bytes().as_slice())
    .fetch_optional(c)
    .await?
    .unwrap_or(false))
}

/// Jobs with a task still in progress whose canonical specification references `hash` (as an
/// input or a checkpoint). Specifications are canonical DAG-CBOR, where a digest is its 32 raw
/// bytes: a byte search is exact up to a SHA-256 collision.
pub async fn jobs_referencing(
    c: &mut PgConnection,
    hash: &Sha256Digest,
) -> Result<Vec<uuid::Uuid>, StorageError> {
    Ok(sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT DISTINCT job_id FROM tasks
         WHERE state NOT IN ('accepted', 'failed', 'cancelled')
           AND position($1::bytea in spec_cbor) > 0",
    )
    .bind(hash.as_bytes().as_slice())
    .fetch_all(c)
    .await?)
}

/// Running collective runs whose configuration or current checkpoint references `hash` (JSON,
/// where a digest is written in lowercase hex).
pub async fn runs_referencing(
    c: &mut PgConnection,
    hash: &Sha256Digest,
) -> Result<Vec<uuid::Uuid>, StorageError> {
    let needle = format!("%{}%", hash.to_hex());
    Ok(sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT id FROM collective_runs
         WHERE state = 'running' AND (config::text LIKE $1 OR checkpoint::text LIKE $1)",
    )
    .bind(needle)
    .fetch_all(c)
    .await?)
}

/// Stop advertising any holder of `hash`.
pub async fn delete_peers_of(
    c: &mut PgConnection,
    hash: &Sha256Digest,
) -> Result<u64, StorageError> {
    let r = sqlx::query("DELETE FROM artifact_peers WHERE hash = $1")
        .bind(hash.as_bytes().as_slice())
        .execute(c)
        .await?;
    Ok(r.rows_affected())
}
