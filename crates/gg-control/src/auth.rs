//! Authentication: bearer tokens (admin/researcher API tokens, worker/validator sessions).
//! Authorization decisions (project membership, admission, roles) are made by handlers.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use gg_core::*;
use gg_protocol::keys::PublicKey;
use gg_storage::queries::identities;

use crate::error::ApiError;
use crate::state::AppState;

/// Authenticated principal.
#[derive(Clone, Debug)]
pub enum Principal {
    /// Operator.
    Admin,
    /// Researcher with admission state.
    Researcher {
        /// Id.
        id: ResearcherId,
        /// Admission.
        admission: AdmissionState,
    },
    /// Worker session.
    Worker {
        /// Id.
        id: WorkerId,
        /// Signing key.
        key: PublicKey,
        /// Authorized flag.
        authorized: bool,
    },
    /// Validator session.
    Validator {
        /// Id.
        id: ValidatorId,
        /// Signing key.
        key: PublicKey,
        /// Authorized flag.
        authorized: bool,
    },
    /// Lab node session (ADR-0041).
    Node {
        /// Id.
        id: NodeId,
        /// Signing key.
        key: PublicKey,
        /// The researcher it works for.
        researcher: ResearcherId,
        /// Not revoked, and its researcher still approved.
        active: bool,
    },
}

impl Principal {
    /// Actor label for audit and idempotency (no secrets).
    pub fn actor(&self) -> String {
        match self {
            Principal::Admin => "admin".into(),
            Principal::Researcher { id, .. } => format!("researcher:{id}"),
            Principal::Worker { id, .. } => format!("worker:{id}"),
            Principal::Validator { id, .. } => format!("validator:{id}"),
            Principal::Node { id, .. } => format!("node:{id}"),
        }
    }
}

/// Token prefixes.
pub const API_TOKEN_PREFIX: &str = "ggt_";
/// Session token prefix.
pub const SESSION_TOKEN_PREFIX: &str = "ggs_";

/// Generate a random token with a prefix (32 random bytes, hex).
pub fn new_token(prefix: &str) -> Result<String, ApiError> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(format!("{prefix}{}", hex::encode(b)))
}

/// Hash of a token as stored.
pub fn token_hash(token: &str) -> Sha256Digest {
    Sha256Digest::of_bytes(token.as_bytes())
}

fn bearer(parts: &Parts) -> Option<String> {
    let v = parts
        .headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let t = v.strip_prefix("Bearer ")?.trim();
    if t.is_empty() || t.len() > 256 {
        None
    } else {
        Some(t.to_owned())
    }
}

/// Resolve a bearer token into a principal.
pub async fn authenticate(state: &AppState, token: &str) -> Result<Principal, ApiError> {
    let h = token_hash(token);
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if token.starts_with(SESSION_TOKEN_PREFIX) {
        let Some(s) = identities::lookup_session(&mut c, &h).await? else {
            return Err(ApiError::Unauthorized);
        };
        return match s.principal_kind.as_str() {
            "worker" => {
                let w = identities::get_worker(&mut c, WorkerId(s.principal_id))
                    .await?
                    .ok_or(ApiError::Unauthorized)?;
                let key = PublicKey(
                    w.public_key
                        .as_slice()
                        .try_into()
                        .map_err(|_| ApiError::Unauthorized)?,
                );
                Ok(Principal::Worker {
                    id: WorkerId(w.id),
                    key,
                    authorized: w.authorized,
                })
            }
            "validator" => {
                let v = identities::get_validator(&mut c, ValidatorId(s.principal_id))
                    .await?
                    .ok_or(ApiError::Unauthorized)?;
                let key = PublicKey(
                    v.public_key
                        .as_slice()
                        .try_into()
                        .map_err(|_| ApiError::Unauthorized)?,
                );
                Ok(Principal::Validator {
                    id: ValidatorId(v.id),
                    key,
                    authorized: v.authorized,
                })
            }
            "node" => {
                let n = gg_storage::queries::nodes::get(&mut c, NodeId(s.principal_id))
                    .await?
                    .ok_or(ApiError::Unauthorized)?;
                let key = PublicKey(
                    n.public_key
                        .as_slice()
                        .try_into()
                        .map_err(|_| ApiError::Unauthorized)?,
                );
                let researcher = ResearcherId(n.researcher_id);
                let approved = identities::get_researcher(&mut c, researcher)
                    .await?
                    .is_some_and(|r| r.admission().ok() == Some(AdmissionState::Approved));
                Ok(Principal::Node {
                    id: NodeId(n.id),
                    key,
                    researcher,
                    active: n.revoked_at.is_none() && approved,
                })
            }
            _ => Err(ApiError::Unauthorized),
        };
    }
    let Some(t) = identities::lookup_token(&mut c, &h).await? else {
        return Err(ApiError::Unauthorized);
    };
    if t.revoked_at.is_some() {
        return Err(ApiError::Unauthorized);
    }
    if let Some(exp) = t.expires_at
        && exp <= ::time::OffsetDateTime::now_utc()
    {
        return Err(ApiError::Unauthorized);
    }
    match t.role.as_str() {
        "admin" => Ok(Principal::Admin),
        "researcher" => {
            let id = ResearcherId(t.researcher_id.ok_or(ApiError::Unauthorized)?);
            let r = identities::get_researcher(&mut c, id)
                .await?
                .ok_or(ApiError::Unauthorized)?;
            Ok(Principal::Researcher {
                id,
                admission: r
                    .admission()
                    .map_err(|e| ApiError::Internal(e.to_string()))?,
            })
        }
        _ => Err(ApiError::Unauthorized),
    }
}

impl FromRequestParts<AppState> for Principal {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer(parts).ok_or(ApiError::Unauthorized)?;
        authenticate(state, &token).await
    }
}

/// Admin-only extractor.
pub struct Admin;

impl FromRequestParts<AppState> for Admin {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        match Principal::from_request_parts(parts, state).await? {
            Principal::Admin => Ok(Admin),
            _ => Err(ApiError::Forbidden("admin role required".into())),
        }
    }
}

/// Approved researcher extractor (authentication + admission, not project rights).
pub struct Researcher(pub ResearcherId);

impl FromRequestParts<AppState> for Researcher {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        match Principal::from_request_parts(parts, state).await? {
            Principal::Researcher {
                id,
                admission: AdmissionState::Approved,
            } => Ok(Researcher(id)),
            Principal::Researcher { admission, .. } => Err(ApiError::Forbidden(format!(
                "researcher admission is {admission}"
            ))),
            _ => Err(ApiError::Forbidden("researcher role required".into())),
        }
    }
}

/// Authorized worker extractor.
pub struct Worker {
    /// Id.
    pub id: WorkerId,
    /// Key.
    pub key: PublicKey,
}

impl FromRequestParts<AppState> for Worker {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        match Principal::from_request_parts(parts, state).await? {
            Principal::Worker {
                id,
                key,
                authorized: true,
            } => Ok(Worker { id, key }),
            Principal::Worker {
                authorized: false, ..
            } => Err(ApiError::Forbidden("worker not authorized".into())),
            _ => Err(ApiError::Forbidden("worker session required".into())),
        }
    }
}

/// A worker session, authorized or not: the only thing it proves is that the caller holds the
/// worker's key (challenge-signed login). Used to attach a machine to a contributor account.
pub struct AnyWorker {
    /// Id.
    pub id: WorkerId,
}

impl FromRequestParts<AppState> for AnyWorker {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        match Principal::from_request_parts(parts, state).await? {
            Principal::Worker { id, .. } => Ok(AnyWorker { id }),
            _ => Err(ApiError::Forbidden("worker session required".into())),
        }
    }
}

/// Authorized validator extractor.
pub struct Validator {
    /// Id.
    pub id: ValidatorId,
    /// Key.
    pub key: PublicKey,
}

impl FromRequestParts<AppState> for Validator {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        match Principal::from_request_parts(parts, state).await? {
            Principal::Validator {
                id,
                key,
                authorized: true,
            } => Ok(Validator { id, key }),
            Principal::Validator {
                authorized: false, ..
            } => Err(ApiError::Forbidden("validator not authorized".into())),
            _ => Err(ApiError::Forbidden("validator session required".into())),
        }
    }
}

/// A lab node session, not revoked, working for an approved researcher (ADR-0041).
pub struct LabNode {
    /// Id.
    pub id: NodeId,
    /// Key.
    pub key: PublicKey,
    /// The researcher it works for.
    pub researcher: ResearcherId,
}

impl FromRequestParts<AppState> for LabNode {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        match Principal::from_request_parts(parts, state).await? {
            Principal::Node {
                id,
                key,
                researcher,
                active: true,
            } => Ok(LabNode {
                id,
                key,
                researcher,
            }),
            Principal::Node { active: false, .. } => Err(ApiError::Forbidden(
                "lab node revoked, or its researcher is not approved".into(),
            )),
            _ => Err(ApiError::Forbidden("lab node session required".into())),
        }
    }
}

/// Check that a researcher has at least `min` role on a project.
pub async fn require_project_role(
    state: &AppState,
    researcher: ResearcherId,
    project: ProjectId,
    min: ProjectRole,
) -> Result<ProjectRole, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let role = identities::membership_role(&mut c, project, researcher)
        .await?
        .ok_or_else(|| ApiError::NotFound("project".into()))?;
    let rank = |r: ProjectRole| match r {
        ProjectRole::Owner => 3,
        ProjectRole::Member => 2,
        ProjectRole::Viewer => 1,
    };
    if rank(role) < rank(min) {
        return Err(ApiError::Forbidden(format!(
            "requires {min} role on project"
        )));
    }
    Ok(role)
}
