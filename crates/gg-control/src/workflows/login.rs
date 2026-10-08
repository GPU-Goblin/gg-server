//! Worker/validator registration and challenge-based login.

use gg_core::*;
use gg_protocol::domain::Domain;
use gg_protocol::keys::PublicKey;
use gg_protocol::messages::LoginChallengeV1;
use gg_protocol::signed::Signed;
use gg_storage::queries::{identities, misc};
use serde::Serialize;

use crate::auth::{SESSION_TOKEN_PREFIX, new_token, token_hash};
use crate::error::ApiError;
use crate::state::AppState;

/// Registration result.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct Registered {
    /// Worker id.
    pub worker_id: WorkerId,
    /// Whether an operator has authorized this key.
    pub authorized: bool,
}

/// Register a worker key (idempotent per key).
pub async fn register_worker(
    state: &AppState,
    key: PublicKey,
    display_name: &str,
) -> Result<Registered, ApiError> {
    if display_name.len() > 64 {
        return Err(ApiError::Unprocessable("display_name too long".into()));
    }
    let mut tx = state.db().begin().await?;
    if let Some(w) = identities::get_worker_by_key(&mut tx, &key.0).await? {
        return Ok(Registered {
            worker_id: WorkerId(w.id),
            authorized: w.authorized,
        });
    }
    let id = WorkerId::new();
    identities::create_worker(&mut tx, id, &key.0, display_name).await?;
    misc::audit(
        &mut tx,
        "worker",
        "worker.register",
        &id.to_string(),
        serde_json::json!({ "public_key": key }),
    )
    .await?;
    tx.commit().await?;
    Ok(Registered {
        worker_id: id,
        authorized: false,
    })
}

/// Challenge response.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct Challenge {
    /// Nonce (hex, 32 bytes).
    pub nonce: String,
    /// Action to sign.
    pub action: String,
    /// Seconds before expiry.
    pub expires_in_secs: i64,
}

/// Issue a single-use challenge for a registered worker or validator key.
pub async fn issue_challenge(
    state: &AppState,
    key: PublicKey,
    action: &str,
) -> Result<Challenge, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let known = match action {
        "worker-login" => identities::get_worker_by_key(&mut c, &key.0)
            .await?
            .is_some(),
        "validator-login" => identities::get_validator_by_key(&mut c, &key.0)
            .await?
            .is_some(),
        "node-login" => gg_storage::queries::nodes::get_by_key(&mut c, &key.0)
            .await?
            .is_some_and(|n| n.revoked_at.is_none()),
        _ => return Err(ApiError::Unprocessable("unknown action".into())),
    };
    if !known {
        return Err(ApiError::NotFound("key is not registered".into()));
    }
    let mut nonce = [0u8; 32];
    getrandom::fill(&mut nonce).map_err(|e| ApiError::Internal(e.to_string()))?;
    let ttl = state.config().challenge_ttl_secs;
    identities::insert_challenge(&mut c, &nonce, action, &key.0, ttl).await?;
    Ok(Challenge {
        nonce: hex::encode(nonce),
        action: action.to_owned(),
        expires_in_secs: ttl,
    })
}

/// Session response.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct Session {
    /// Bearer token (shown once).
    pub token: String,
    /// Seconds before expiry.
    pub expires_in_secs: i64,
    /// Principal id.
    pub principal_id: uuid::Uuid,
    /// `worker` or `validator`.
    pub principal_kind: String,
}

/// Exchange a signed challenge for a short session.
pub async fn create_session(
    state: &AppState,
    signed: Signed<LoginChallengeV1>,
) -> Result<Session, ApiError> {
    let body = &signed.body;
    body.validate_structure()?;
    if body.network != *state.network() {
        return Err(ApiError::Unprocessable("network mismatch".into()));
    }
    if signed.signer != body.public_key {
        return Err(ApiError::Forbidden(
            "signer differs from the challenged key".into(),
        ));
    }
    signed.verify(state.network(), Domain::Login)?;
    let mut tx = state.db().begin().await?;
    if !identities::consume_challenge(&mut tx, &body.nonce, &body.action, &body.public_key.0)
        .await?
    {
        return Err(ApiError::Forbidden(
            "challenge unknown, expired or already used".into(),
        ));
    }
    let (kind, id) = match body.action.as_str() {
        "worker-login" => {
            let w = identities::get_worker_by_key(&mut tx, &body.public_key.0)
                .await?
                .ok_or(ApiError::Unauthorized)?;
            ("worker", w.id)
        }
        "validator-login" => {
            let v = identities::get_validator_by_key(&mut tx, &body.public_key.0)
                .await?
                .ok_or(ApiError::Unauthorized)?;
            ("validator", v.id)
        }
        "node-login" => {
            let n = gg_storage::queries::nodes::get_by_key(&mut tx, &body.public_key.0)
                .await?
                .filter(|n| n.revoked_at.is_none())
                .ok_or(ApiError::Unauthorized)?;
            gg_storage::queries::nodes::touch(&mut tx, NodeId(n.id)).await?;
            ("node", n.id)
        }
        _ => return Err(ApiError::Unprocessable("unknown action".into())),
    };
    let token = new_token(SESSION_TOKEN_PREFIX)?;
    let ttl = state.config().session_ttl_secs;
    identities::create_session(&mut tx, &token_hash(&token), kind, id, ttl).await?;
    tx.commit().await?;
    Ok(Session {
        token,
        expires_in_secs: ttl,
        principal_id: id,
        principal_kind: kind.to_owned(),
    })
}
