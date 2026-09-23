//! Contributor accounts, their verification codes, portal sessions and enrollment tokens (ADR-0030).

use gg_core::*;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::StorageError;

/// A contributor account.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ContributorRow {
    /// Id.
    pub id: Uuid,
    /// Verified address.
    pub email: String,
    /// Created.
    pub created_at: ::time::OffsetDateTime,
    /// Suspended by an operator, if so.
    pub suspended_at: Option<::time::OffsetDateTime>,
    /// Why.
    pub suspended_reason: String,
}

const COLS: &str = "id, email, created_at, suspended_at, suspended_reason";

/// Record a verification code sent to `email` (hashed together with the address).
pub async fn insert_code(
    c: &mut PgConnection,
    email: &str,
    code_hash: &Sha256Digest,
    ttl_secs: i64,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO contributor_codes (id, email, code_hash, expires_at)
         VALUES ($1, $2, $3, now() + make_interval(secs => $4))",
    )
    .bind(Uuid::now_v7())
    .bind(email)
    .bind(code_hash.as_bytes().as_slice())
    .bind(ttl_secs as f64)
    .execute(c)
    .await?;
    Ok(())
}

/// Codes sent to `email` during the last `window_secs` seconds.
pub async fn count_recent_codes(
    c: &mut PgConnection,
    email: &str,
    window_secs: i64,
) -> Result<i64, StorageError> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM contributor_codes
         WHERE lower(email) = lower($1) AND created_at > now() - make_interval(secs => $2)",
    )
    .bind(email)
    .bind(window_secs as f64)
    .fetch_one(c)
    .await?)
}

/// Check a code against the latest live one sent to `email`. Every check counts as an attempt;
/// a code is dead after `max_attempts` checks, after it expires, or once it is used.
pub async fn check_code(
    c: &mut PgConnection,
    email: &str,
    code_hash: &Sha256Digest,
    max_attempts: i32,
) -> Result<bool, StorageError> {
    let row: Option<(Uuid, Vec<u8>)> = sqlx::query_as(
        "SELECT id, code_hash FROM contributor_codes
         WHERE lower(email) = lower($1) AND consumed_at IS NULL AND expires_at > now()
           AND attempts < $2
         ORDER BY created_at DESC LIMIT 1 FOR UPDATE",
    )
    .bind(email)
    .bind(max_attempts)
    .fetch_optional(&mut *c)
    .await?;
    let Some((id, stored)) = row else {
        return Ok(false);
    };
    let ok = subtle_eq(&stored, code_hash.as_bytes());
    if ok {
        sqlx::query("UPDATE contributor_codes SET consumed_at = now(), attempts = attempts + 1 WHERE id = $1")
            .bind(id)
            .execute(&mut *c)
            .await?;
    } else {
        sqlx::query("UPDATE contributor_codes SET attempts = attempts + 1 WHERE id = $1")
            .bind(id)
            .execute(&mut *c)
            .await?;
    }
    Ok(ok)
}

fn subtle_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The account of `email`, if any (case-insensitive).
pub async fn get_by_email(
    c: &mut PgConnection,
    email: &str,
) -> Result<Option<ContributorRow>, StorageError> {
    Ok(
        sqlx::query_as::<_, ContributorRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {COLS} FROM contributors WHERE lower(email) = lower($1)"
        )))
        .bind(email)
        .fetch_optional(c)
        .await?,
    )
}

/// An account by id.
pub async fn get(c: &mut PgConnection, id: Uuid) -> Result<Option<ContributorRow>, StorageError> {
    Ok(
        sqlx::query_as::<_, ContributorRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {COLS} FROM contributors WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(c)
        .await?,
    )
}

/// Create an account for a verified address.
pub async fn create(c: &mut PgConnection, id: Uuid, email: &str) -> Result<(), StorageError> {
    sqlx::query("INSERT INTO contributors (id, email, verified_at) VALUES ($1, $2, now())")
        .bind(id)
        .bind(email)
        .execute(c)
        .await?;
    Ok(())
}

/// All accounts, oldest first.
pub async fn list(c: &mut PgConnection) -> Result<Vec<ContributorRow>, StorageError> {
    Ok(
        sqlx::query_as::<_, ContributorRow>(sqlx::AssertSqlSafe(format!(
            "SELECT {COLS} FROM contributors ORDER BY created_at"
        )))
        .fetch_all(c)
        .await?,
    )
}

/// Suspend or reinstate an account. Returns false if unknown.
pub async fn set_suspended(
    c: &mut PgConnection,
    id: Uuid,
    suspended: bool,
    reason: &str,
) -> Result<bool, StorageError> {
    let r = if suspended {
        sqlx::query(
            "UPDATE contributors SET suspended_at = now(), suspended_reason = $2 WHERE id = $1",
        )
        .bind(id)
        .bind(reason)
        .execute(c)
        .await?
    } else {
        sqlx::query(
            "UPDATE contributors SET suspended_at = NULL, suspended_reason = '' WHERE id = $1",
        )
        .bind(id)
        .execute(c)
        .await?
    };
    Ok(r.rows_affected() == 1)
}

/// Open a portal session.
pub async fn create_session(
    c: &mut PgConnection,
    session_hash: &Sha256Digest,
    contributor: Uuid,
    ttl_secs: i64,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO contributor_sessions (session_hash, contributor_id, expires_at)
         VALUES ($1, $2, now() + make_interval(secs => $3))",
    )
    .bind(session_hash.as_bytes().as_slice())
    .bind(contributor)
    .bind(ttl_secs as f64)
    .execute(c)
    .await?;
    Ok(())
}

/// The account behind a live session.
pub async fn lookup_session(
    c: &mut PgConnection,
    session_hash: &Sha256Digest,
) -> Result<Option<Uuid>, StorageError> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT contributor_id FROM contributor_sessions
         WHERE session_hash = $1 AND revoked_at IS NULL AND expires_at > now()",
    )
    .bind(session_hash.as_bytes().as_slice())
    .fetch_optional(c)
    .await?)
}

/// End a session.
pub async fn revoke_session(
    c: &mut PgConnection,
    session_hash: &Sha256Digest,
) -> Result<(), StorageError> {
    sqlx::query("UPDATE contributor_sessions SET revoked_at = now() WHERE session_hash = $1")
        .bind(session_hash.as_bytes().as_slice())
        .execute(c)
        .await?;
    Ok(())
}

/// Revoke every session and enrollment token of an account.
pub async fn revoke_all(c: &mut PgConnection, contributor: Uuid) -> Result<(), StorageError> {
    sqlx::query("UPDATE contributor_sessions SET revoked_at = now() WHERE contributor_id = $1 AND revoked_at IS NULL")
        .bind(contributor)
        .execute(&mut *c)
        .await?;
    sqlx::query("UPDATE enrollment_tokens SET revoked_at = now() WHERE contributor_id = $1 AND revoked_at IS NULL")
        .bind(contributor)
        .execute(&mut *c)
        .await?;
    Ok(())
}

/// Replace the account's enrollment token: earlier ones stop working.
pub async fn replace_token(
    c: &mut PgConnection,
    contributor: Uuid,
    token_hash: &Sha256Digest,
) -> Result<(), StorageError> {
    sqlx::query("UPDATE enrollment_tokens SET revoked_at = now() WHERE contributor_id = $1 AND revoked_at IS NULL")
        .bind(contributor)
        .execute(&mut *c)
        .await?;
    sqlx::query("INSERT INTO enrollment_tokens (token_hash, contributor_id) VALUES ($1, $2)")
        .bind(token_hash.as_bytes().as_slice())
        .bind(contributor)
        .execute(&mut *c)
        .await?;
    Ok(())
}

/// The active account of a live enrollment token (none if revoked or suspended).
pub async fn account_of_token(
    c: &mut PgConnection,
    token_hash: &Sha256Digest,
) -> Result<Option<Uuid>, StorageError> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT t.contributor_id FROM enrollment_tokens t JOIN contributors c ON c.id = t.contributor_id
         WHERE t.token_hash = $1 AND t.revoked_at IS NULL AND c.suspended_at IS NULL",
    )
    .bind(token_hash.as_bytes().as_slice())
    .fetch_optional(c)
    .await?)
}

/// Attach a worker to an account and authorize it. Returns false if the worker already belongs to
/// another account.
pub async fn attach_worker(
    c: &mut PgConnection,
    worker: WorkerId,
    contributor: Uuid,
) -> Result<bool, StorageError> {
    let r = sqlx::query(
        "UPDATE workers SET contributor_id = $2, authorized = true
         WHERE id = $1 AND (contributor_id IS NULL OR contributor_id = $2)",
    )
    .bind(worker.0)
    .bind(contributor)
    .execute(c)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// A machine of an account, as its owner sees it.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct MachineRow {
    /// Worker id.
    pub id: Uuid,
    /// Name given at registration.
    pub display_name: String,
    /// Authorized.
    pub authorized: bool,
    /// Last contact.
    pub last_seen_at: Option<::time::OffsetDateTime>,
    /// Tasks it computed that were accepted.
    pub accepted: i64,
}

/// The machines of an account.
pub async fn machines(
    c: &mut PgConnection,
    contributor: Uuid,
) -> Result<Vec<MachineRow>, StorageError> {
    Ok(sqlx::query_as::<_, MachineRow>(
        "SELECT w.id, w.display_name, w.authorized, w.last_seen_at,
                (SELECT count(*) FROM attempts a WHERE a.worker_id = w.id AND a.state = 'accepted') AS accepted
         FROM workers w WHERE w.contributor_id = $1 ORDER BY w.created_at",
    )
    .bind(contributor)
    .fetch_all(c)
    .await?)
}

/// Deauthorize every machine of an account; returns their ids.
pub async fn deauthorize_machines(
    c: &mut PgConnection,
    contributor: Uuid,
) -> Result<Vec<Uuid>, StorageError> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "UPDATE workers SET authorized = false WHERE contributor_id = $1 RETURNING id",
    )
    .bind(contributor)
    .fetch_all(c)
    .await?)
}
