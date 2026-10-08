//! Contributor accounts (ADR-0030): sign-up by an e-mail code, no admission; machines attached by
//! an enrollment token are authorized automatically; suspending an account stops all of them.

use gg_core::*;
use gg_storage::queries::{contributors, identities, misc};
use serde::Serialize;

use crate::auth::{new_token, token_hash};
use crate::error::ApiError;
use crate::state::AppState;

/// Prefix of enrollment tokens.
pub const ENROLLMENT_TOKEN_PREFIX: &str = "gge_";
/// Checks allowed per verification code.
pub const MAX_CODE_ATTEMPTS: i32 = 5;
/// Lifetime of a verification code.
pub const CODE_TTL_SECS: i64 = 900;

/// Hash of a verification code, bound to the address it was sent to.
pub fn code_hash(email: &str, code: &str) -> Sha256Digest {
    Sha256Digest::of_bytes(
        format!(
            "gg-contributor-code/v1\n{}\n{}",
            email.to_ascii_lowercase(),
            code.trim()
        )
        .as_bytes(),
    )
}

/// An eight-digit verification code.
pub fn new_code() -> Result<String, ApiError> {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(format!("{:08}", u64::from_le_bytes(b) % 100_000_000))
}

/// Send a verification code to `email`, unless the hourly limit for that address is reached.
/// Returns whether a code was sent (callers answer the same either way).
pub async fn send_code(state: &AppState, email: &str) -> Result<bool, ApiError> {
    let limit = state.config().email_login.max_links_per_hour;
    let mut c = state.db().pool().acquire().await?;
    if contributors::count_recent_codes(&mut c, email, 3600).await? >= limit {
        return Ok(false);
    }
    let code = new_code()?;
    contributors::insert_code(&mut c, email, &code_hash(email, &code), CODE_TTL_SECS).await?;
    drop(c);
    let body = format!(
        "Your GPU Goblin verification code is:\n\n    {code}\n\nIt is valid for {} minutes. If you did not ask to lend a machine to GPU Goblin, ignore this\nmessage: nothing happens without the code.\n",
        CODE_TTL_SECS / 60
    );
    state
        .mailer()
        .send(email, "Your GPU Goblin verification code", &body)
        .await?;
    Ok(true)
}

/// Check a code; on success return the account of `email`, created on first verification.
pub async fn verify_code(
    state: &AppState,
    email: &str,
    code: &str,
) -> Result<Option<uuid::Uuid>, ApiError> {
    let mut tx = state.db().begin().await?;
    if !contributors::check_code(&mut tx, email, &code_hash(email, code), MAX_CODE_ATTEMPTS).await?
    {
        tx.commit().await?; // attempts are counted even when the code is wrong
        return Ok(None);
    }
    let id = match contributors::get_by_email(&mut tx, email).await? {
        Some(a) => a.id,
        None => {
            let id = uuid::Uuid::now_v7();
            contributors::create(&mut tx, id, email).await?;
            misc::audit(
                &mut tx,
                "portal",
                "contributor.signup",
                &id.to_string(),
                serde_json::json!({ "email": email }),
            )
            .await?;
            id
        }
    };
    tx.commit().await?;
    Ok(Some(id))
}

/// Issue a new enrollment token for an account; earlier ones stop working. Shown once.
pub async fn new_enrollment_token(
    state: &AppState,
    contributor: uuid::Uuid,
) -> Result<String, ApiError> {
    let token = new_token(ENROLLMENT_TOKEN_PREFIX)?;
    let mut tx = state.db().begin().await?;
    contributors::replace_token(&mut tx, contributor, &token_hash(&token)).await?;
    misc::audit(
        &mut tx,
        "portal",
        "contributor.token",
        &contributor.to_string(),
        serde_json::json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(token)
}

/// What an enrollment did.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct Enrolled {
    /// The machine.
    pub worker_id: WorkerId,
    /// It may now claim tasks.
    pub authorized: bool,
}

/// Attach the calling machine (its key proven by its session) to the account of `token`.
pub async fn enroll(state: &AppState, worker: WorkerId, token: &str) -> Result<Enrolled, ApiError> {
    if !token.starts_with(ENROLLMENT_TOKEN_PREFIX) || token.len() > 128 {
        return Err(ApiError::Unprocessable("not an enrollment code".into()));
    }
    let mut tx = state.db().begin().await?;
    let Some(account) = contributors::account_of_token(&mut tx, &token_hash(token)).await? else {
        return Err(ApiError::Forbidden(
            "enrollment code unknown or replaced, or its account is suspended".into(),
        ));
    };
    if !contributors::attach_worker(&mut tx, worker, account).await? {
        return Err(ApiError::Conflict(
            "this machine already belongs to another account".into(),
        ));
    }
    misc::audit(
        &mut tx,
        "worker",
        "contributor.enroll",
        &worker.to_string(),
        serde_json::json!({ "contributor_id": account }),
    )
    .await?;
    tx.commit().await?;
    Ok(Enrolled {
        worker_id: worker,
        authorized: true,
    })
}

/// Suspend an account (every machine deauthorized, sessions and tokens revoked, no longer
/// advertised to peers) or reinstate it (its machines are authorized again).
pub async fn set_suspended(
    state: &AppState,
    actor: &str,
    contributor: uuid::Uuid,
    suspended: bool,
    reason: &str,
) -> Result<usize, ApiError> {
    let mut tx = state.db().begin().await?;
    let machines = set_suspended_in(&mut tx, actor, contributor, suspended, reason).await?;
    tx.commit().await?;
    Ok(machines)
}

/// [`set_suspended`] within a transaction.
pub(crate) async fn set_suspended_in(
    tx: &mut gg_storage::PgConnection,
    actor: &str,
    contributor: uuid::Uuid,
    suspended: bool,
    reason: &str,
) -> Result<usize, ApiError> {
    if !contributors::set_suspended(tx, contributor, suspended, reason).await? {
        return Err(ApiError::NotFound("contributor".into()));
    }
    let mut machines = 0;
    if suspended {
        contributors::revoke_all(tx, contributor).await?;
        for w in contributors::deauthorize_machines(tx, contributor).await? {
            identities::revoke_sessions(tx, "worker", w).await?;
            gg_storage::queries::p2p::delete_peers_of_worker(tx, WorkerId(w)).await?;
            machines += 1;
        }
    } else {
        for m in contributors::machines(tx, contributor).await? {
            identities::set_worker_authorized(tx, WorkerId(m.id), true).await?;
            machines += 1;
        }
    }
    misc::audit(
        tx,
        actor,
        "contributor.suspend",
        &contributor.to_string(),
        serde_json::json!({ "suspended": suspended, "reason": reason, "machines": machines }),
    )
    .await?;
    Ok(machines)
}
