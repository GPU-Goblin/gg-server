//! Idempotency keys, outbox and audit events.

use gg_core::*;
use sqlx::PgConnection;

use crate::StorageError;
use crate::models::{IdempotencyRow, OutboxRow};

/// Look up an idempotency record.
pub async fn get_idempotency(
    c: &mut PgConnection,
    actor: &str,
    operation: &str,
    key: &str,
) -> Result<Option<IdempotencyRow>, StorageError> {
    Ok(sqlx::query_as::<_, IdempotencyRow>(
        "SELECT request_hash, status, response FROM idempotency_keys WHERE actor = $1 AND operation = $2 AND key = $3",
    )
    .bind(actor)
    .bind(operation)
    .bind(key)
    .fetch_optional(c)
    .await?)
}

/// Store an idempotency record (fails on conflict; callers look up first inside the transaction).
pub async fn put_idempotency(
    c: &mut PgConnection,
    actor: &str,
    operation: &str,
    key: &str,
    request_hash: &Sha256Digest,
    status: i32,
    response: &serde_json::Value,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO idempotency_keys (actor, operation, key, request_hash, status, response) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(actor)
    .bind(operation)
    .bind(key)
    .bind(request_hash.as_bytes().as_slice())
    .bind(status)
    .bind(response)
    .execute(c)
    .await?;
    Ok(())
}

/// Append an outbox event.
pub async fn outbox_insert(
    c: &mut PgConnection,
    topic: &str,
    payload: &serde_json::Value,
) -> Result<(), StorageError> {
    sqlx::query("INSERT INTO outbox (topic, payload) VALUES ($1, $2)")
        .bind(topic)
        .bind(payload)
        .execute(c)
        .await?;
    Ok(())
}

/// Fetch undelivered events (locked for the dispatcher).
pub async fn outbox_pending(
    c: &mut PgConnection,
    limit: i64,
) -> Result<Vec<OutboxRow>, StorageError> {
    Ok(sqlx::query_as::<_, OutboxRow>(
        "SELECT id, topic, payload, created_at FROM outbox WHERE delivered_at IS NULL ORDER BY id LIMIT $1 FOR UPDATE SKIP LOCKED",
    )
    .bind(limit)
    .fetch_all(c)
    .await?)
}

/// Mark events delivered.
pub async fn outbox_mark_delivered(c: &mut PgConnection, ids: &[i64]) -> Result<(), StorageError> {
    sqlx::query("UPDATE outbox SET delivered_at = now() WHERE id = ANY($1)")
        .bind(ids)
        .execute(c)
        .await?;
    Ok(())
}

/// Count undelivered events.
pub async fn outbox_pending_count(c: &mut PgConnection) -> Result<i64, StorageError> {
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM outbox WHERE delivered_at IS NULL")
        .fetch_one(c)
        .await?;
    Ok(n)
}

/// Append an audit event.
pub async fn audit(
    c: &mut PgConnection,
    actor: &str,
    action: &str,
    subject: &str,
    detail: serde_json::Value,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO audit_events (id, actor, action, subject, detail) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(AuditEventId::new().0)
    .bind(actor)
    .bind(action)
    .bind(subject)
    .bind(detail)
    .execute(c)
    .await?;
    Ok(())
}

/// Recent audit events.
pub async fn list_audit(
    c: &mut PgConnection,
    limit: i64,
) -> Result<Vec<(String, String, String, serde_json::Value)>, StorageError> {
    Ok(sqlx::query_as(
        "SELECT actor, action, subject, detail FROM audit_events ORDER BY created_at DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(c)
    .await?)
}
