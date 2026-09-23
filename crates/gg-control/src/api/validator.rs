//! Validator routes.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use gg_core::*;
use gg_protocol::keys::PublicKey;
use gg_protocol::messages::{LoginChallengeV1, ResultSubmissionV1, VerificationDecisionV1};
use gg_protocol::signed::Signed;
use gg_storage::queries::{jobs, tasks};
use serde::Deserialize;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::api::researcher::stream_object_ranged;
use crate::auth::Validator;
use crate::error::ApiError;
use crate::quota::lease_covered;
use crate::state::AppState;
use crate::workflows::decide::{
    DecisionApplied, VerificationWork, apply_decision, claim_verification_wait,
};
use crate::workflows::decode_spec;
use crate::workflows::login::{Challenge, Session, create_session, issue_challenge};

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ChallengeRequest {
    /// Ed25519 public key (hex).
    pub public_key: String,
}

#[utoipa::path(post, path = "/validator/challenge", tag = "validator", request_body = ChallengeRequest, responses((status = 200, body = Challenge)))]
async fn challenge(
    State(state): State<AppState>,
    Json(req): Json<ChallengeRequest>,
) -> Result<Json<Challenge>, ApiError> {
    let key: PublicKey = req.public_key.parse()?;
    Ok(Json(issue_challenge(&state, key, "validator-login").await?))
}

#[utoipa::path(post, path = "/validator/session", tag = "validator", request_body = Object, responses((status = 200, body = Session)))]
async fn session(
    State(state): State<AppState>,
    Json(signed): Json<Signed<LoginChallengeV1>>,
) -> Result<Json<Session>, ApiError> {
    if signed.body.action != "validator-login" {
        return Err(ApiError::Unprocessable(
            "action must be validator-login".into(),
        ));
    }
    Ok(Json(create_session(&state, signed).await?))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ClaimRequest {
    #[serde(default)]
    pub wait_secs: u64,
    /// Numeric profile the validator recomputes model tasks with (ADR-0031); absent: the CPU
    /// reference profile. Only a CPU validator is offered engine checks, and a decision states the
    /// profile it was computed with, which the server checks.
    #[serde(default)]
    pub reference_profile: Option<String>,
    /// Take only secondary verifications, audits and engine checks, never a primary one: a CPU
    /// validator next to GPU validators that take the primary verifications (ADR-0031).
    #[serde(default)]
    pub secondary_only: bool,
}

#[utoipa::path(post, path = "/validator/claim", tag = "validator", request_body = ClaimRequest, responses((status = 200, body = VerificationWork), (status = 204, description = "nothing to verify")))]
async fn claim(
    State(state): State<AppState>,
    v: Validator,
    Json(req): Json<ClaimRequest>,
) -> Result<Response, ApiError> {
    let cpu_reference = req
        .reference_profile
        .as_deref()
        .is_none_or(|p| p == gg_compute::profiles::REFERENCE_PROFILE);
    match claim_verification_wait(
        &state,
        v.id,
        req.wait_secs,
        cpu_reference,
        req.secondary_only,
    )
    .await?
    {
        Some(w) => Ok(Json(w).into_response()),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

#[utoipa::path(post, path = "/validator/verifications/{id}/decision", tag = "validator", request_body = Object, responses((status = 200, body = DecisionApplied)))]
async fn decision(
    State(state): State<AppState>,
    v: Validator,
    Path(id): Path<uuid::Uuid>,
    Json(signed): Json<Signed<VerificationDecisionV1>>,
) -> Result<Json<DecisionApplied>, ApiError> {
    let applied =
        crate::workflows::retrying(|| apply_decision(&state, v.id, v.key, id, signed.clone()))
            .await?;
    if applied.newly_recorded {
        let i = match applied.outcome {
            VerificationOutcome::Accepted => 0,
            VerificationOutcome::Rejected => 1,
            VerificationOutcome::Inconclusive => 2,
        };
        state.metrics().decisions[i].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(Json(applied))
}

#[utoipa::path(post, path = "/validator/verifications/{id}/release", tag = "validator", responses((status = 200, body = Object)))]
async fn release(
    State(state): State<AppState>,
    v: Validator,
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let released = crate::workflows::decide::release_verification(&state, v.id, id).await?;
    Ok(Json(serde_json::json!({ "released": released })))
}

/// Tasks examined for the inputs validators may prefetch.
const UPCOMING_TASKS: i64 = 64;
/// Artifacts announced at most.
const UPCOMING_MAX: usize = 16;

#[derive(Deserialize, utoipa::ToSchema)]
pub struct UpcomingRequest {
    /// Numeric profile the validator recomputes model tasks with; only the tasks it may verify are
    /// considered. Absent: the CPU reference profile.
    #[serde(default)]
    pub reference_profile: Option<String>,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct Upcoming {
    /// Durably stored inputs of tasks being computed or awaiting verification, largest first.
    #[schema(value_type = Object)]
    pub artifacts: Vec<gg_protocol::messages::ArtifactRef>,
}

/// Inputs of the tasks being computed or awaiting their verification, stored durably, largest
/// first: a validator downloads them while the worker computes, so that a round's checkpoint is
/// already there when the submission arrives (ADR-0037). `profile`: only tasks it may verify.
async fn upcoming_inputs(
    state: &AppState,
    profile: Option<&str>,
) -> Result<Vec<gg_protocol::messages::ArtifactRef>, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut out: Vec<gg_protocol::messages::ArtifactRef> = Vec::new();
    for (kind, spec) in tasks::list_upcoming_specs(&mut c, UPCOMING_TASKS).await? {
        if profile.is_some_and(|p| !gg_compute::profiles::reference_admitted_for(&kind, p)) {
            continue;
        }
        for r in decode_spec(&spec)?.definition.inputs.into_values() {
            if !out.iter().any(|o| o.hash == r.hash) && state.objects().exists(&r.hash) {
                out.push(r);
            }
        }
    }
    out.sort_by_key(|r| std::cmp::Reverse(r.size));
    Ok(out)
}

#[utoipa::path(post, path = "/validator/upcoming", tag = "validator", request_body = UpcomingRequest, responses((status = 200, body = Upcoming)))]
async fn upcoming(
    State(state): State<AppState>,
    _v: Validator,
    Json(req): Json<UpcomingRequest>,
) -> Result<Json<Upcoming>, ApiError> {
    let profile = req
        .reference_profile
        .unwrap_or_else(|| gg_compute::profiles::REFERENCE_PROFILE.to_owned());
    let mut artifacts = upcoming_inputs(&state, Some(&profile)).await?;
    artifacts.truncate(UPCOMING_MAX);
    Ok(Json(Upcoming { artifacts }))
}

/// Whether `hash` is an input or output of a verification held by this validator, or an input of
/// a task it may soon verify (ADR-0037).
async fn validator_may_read(
    state: &AppState,
    validator: ValidatorId,
    hash: &Sha256Digest,
) -> Result<bool, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    for v in tasks::list_in_progress_verifications(&mut c, validator).await? {
        if let Some(t) = jobs::get_task(&mut c, TaskId(v.task_id)).await? {
            let spec = decode_spec(&t.spec_cbor)?;
            if spec.definition.inputs.values().any(|r| r.hash == *hash) {
                return Ok(true);
            }
        }
        if let Some(s) = tasks::get_submission(&mut c, SubmissionId(v.submission_id)).await? {
            let signed: Signed<ResultSubmissionV1> =
                Signed::from_canonical_bytes(&s.submission_cbor)
                    .map_err(|e| ApiError::Internal(e.to_string()))?;
            if signed.body.outputs.values().any(|r| r.hash == *hash) {
                return Ok(true);
            }
        }
    }
    drop(c);
    Ok(upcoming_inputs(state, None)
        .await?
        .iter()
        .any(|r| r.hash == *hash))
}

async fn download(
    State(state): State<AppState>,
    v: Validator,
    Path(hash): Path<Sha256Digest>,
    headers: axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    if !validator_may_read(&state, v.id, &hash).await? {
        return Err(ApiError::NotFound("artifact".into()));
    }
    let range = headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_owned());
    // An input or output of a verification the caller holds (`validator_may_read`).
    stream_object_ranged(&state, &hash, range.as_deref())
        .await
        .map(lease_covered)
}

/// JSON routes.
pub fn json_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(challenge))
        .routes(routes!(session))
        .routes(routes!(claim))
        .routes(routes!(decision))
        .routes(routes!(release))
        .routes(routes!(upcoming))
}

/// Streaming routes.
pub fn upload_routes() -> Router<AppState> {
    Router::new().route("/validator/artifacts/{hash}", get(download))
}
