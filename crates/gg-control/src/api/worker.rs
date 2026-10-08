//! Worker routes.

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use gg_core::*;
use gg_protocol::keys::PublicKey;
use gg_protocol::messages::{LoginChallengeV1, ResultSubmissionV1};
use gg_protocol::signed::Signed;
use gg_storage::queries::{artifacts, identities, jobs, pending, tasks};
use serde::{Deserialize, Serialize};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::api::researcher::{
    ReceiptExport, UploadQuery, export_receipt, store_upload, stream_object_ranged,
};
use crate::auth::Worker;
use crate::error::ApiError;
use crate::quota::lease_covered;
use crate::state::AppState;
use crate::workflows::claim::{Claimed, abandon, claim_wait, heartbeat};
use crate::workflows::decode_spec;
use crate::workflows::login::{
    Challenge, Registered, Session, create_session, issue_challenge, register_worker,
};
use crate::workflows::submit::{Submitted, submit_result};

#[derive(Deserialize, utoipa::ToSchema)]
pub struct RegisterRequest {
    /// Ed25519 public key (hex).
    pub public_key: String,
    #[serde(default)]
    pub display_name: String,
}

#[utoipa::path(post, path = "/worker/register", tag = "worker", request_body = RegisterRequest, responses((status = 200, body = Registered)))]
async fn register(
    State(state): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<Registered>, ApiError> {
    let key: PublicKey = req.public_key.parse()?;
    Ok(Json(register_worker(&state, key, &req.display_name).await?))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct EnrollRequest {
    /// Enrollment code of a contributor account (`gge_…`), from the contributor portal.
    pub enrollment_token: String,
}

/// Attach this machine to a contributor account and authorize it (ADR-0030). The session proves the
/// caller holds the machine's key, so nobody can attach someone else's machine.
#[utoipa::path(post, path = "/worker/enroll", tag = "worker", request_body = EnrollRequest, responses((status = 200, body = crate::workflows::contributors::Enrolled)))]
async fn enroll(
    State(state): State<AppState>,
    w: crate::auth::AnyWorker,
    Json(req): Json<EnrollRequest>,
) -> Result<Json<crate::workflows::contributors::Enrolled>, ApiError> {
    Ok(Json(
        crate::workflows::contributors::enroll(&state, w.id, req.enrollment_token.trim()).await?,
    ))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ChallengeRequest {
    /// Ed25519 public key (hex).
    pub public_key: String,
}

#[utoipa::path(post, path = "/worker/challenge", tag = "worker", request_body = ChallengeRequest, responses((status = 200, body = Challenge)))]
async fn challenge(
    State(state): State<AppState>,
    Json(req): Json<ChallengeRequest>,
) -> Result<Json<Challenge>, ApiError> {
    let key: PublicKey = req.public_key.parse()?;
    Ok(Json(issue_challenge(&state, key, "worker-login").await?))
}

#[utoipa::path(post, path = "/worker/session", tag = "worker", request_body = Object, responses((status = 200, body = Session)))]
async fn session(
    State(state): State<AppState>,
    Json(signed): Json<Signed<LoginChallengeV1>>,
) -> Result<Json<Session>, ApiError> {
    if signed.body.action != "worker-login" {
        return Err(ApiError::Unprocessable(
            "action must be worker-login".into(),
        ));
    }
    Ok(Json(create_session(&state, signed).await?))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct WorkerMe {
    pub worker_id: WorkerId,
    pub public_key: String,
    pub authorized: bool,
    pub earned: Credits,
    /// Computations of this worker the grid accepted (receipts).
    pub accepted: u64,
    pub active_attempts: Vec<AttemptId>,
    /// Source address the control plane sees for this worker. A worker whose NAT preserves the port
    /// (or that has a port forward) can announce `observed_ip:local_port` instead of guessing its
    /// own reachable address (ADR-0013).
    pub observed_ip: String,
}

#[utoipa::path(get, path = "/worker/me", tag = "worker", responses((status = 200, body = WorkerMe)))]
async fn me(
    State(state): State<AppState>,
    w: Worker,
    axum::Extension(crate::client_ip::ClientIp(peer)): axum::Extension<crate::client_ip::ClientIp>,
) -> Result<Json<WorkerMe>, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let earned = gg_ledger::worker_earned(&mut c, w.id).await?;
    let accepted: i64 = sqlx::query_scalar("SELECT count(*) FROM receipts WHERE worker_id = $1")
        .bind(w.id.0)
        .fetch_one(&mut *c)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let active = tasks::list_active_attempts_of_worker(&mut c, w.id)
        .await?
        .into_iter()
        .map(|a| AttemptId(a.id))
        .collect();
    Ok(Json(WorkerMe {
        worker_id: w.id,
        public_key: w.key.to_hex(),
        authorized: true,
        earned,
        accepted: accepted.max(0) as u64,
        active_attempts: active,
        observed_ip: peer.to_string(),
    }))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct PayoutRequest {
    /// Address the contributor wants to be paid at, on the chain the operator settles on. It ends
    /// up inside a signed reward leaf, so the contributor declares it: nobody declares it for them.
    pub address: String,
}

/// Declare the payout address of this worker (ADR-0015). A contribution with no declared address is
/// never committed to a batch.
#[utoipa::path(post, path = "/worker/payout", tag = "worker", request_body = PayoutRequest, responses((status = 204)))]
async fn set_payout(
    State(state): State<AppState>,
    w: Worker,
    Json(req): Json<PayoutRequest>,
) -> Result<StatusCode, ApiError> {
    let address = req.address.trim();
    if address.is_empty() || address.len() > 128 || address.chars().any(|c| c.is_whitespace()) {
        return Err(ApiError::Unprocessable(
            "address must be 1..=128 bytes without whitespace".into(),
        ));
    }
    let mut tx = state.db().begin().await?;
    if !identities::set_payout_address(&mut tx, w.id, address).await? {
        return Err(ApiError::NotFound("worker".into()));
    }
    gg_storage::queries::misc::audit(
        &mut tx,
        &format!("worker:{}", w.id),
        "worker.payout_address",
        &w.id.to_string(),
        serde_json::json!({ "address": address }),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ClaimRequest {
    /// Compute kinds this worker can execute.
    pub kinds: Vec<String>,
    /// Long-poll seconds (bounded by the server).
    #[serde(default)]
    pub wait_secs: u64,
    /// Host memory the executor may use, bytes. Absent: an earlier worker, not filtered on memory.
    #[serde(default)]
    pub ram_bytes: Option<u64>,
    /// Accelerator memory the executor may use (host memory for a CPU engine), bytes.
    #[serde(default)]
    pub device_memory_bytes: Option<u64>,
    /// Operator profile understood, `gg-ops/N`. Absent: `gg-ops/1`.
    #[serde(default)]
    pub profile: Option<String>,
    /// Engine, informative (`cpu`, `gpu:0`).
    #[serde(default)]
    pub engine: Option<String>,
    /// Version of the worker (`x.y.z`), checked against `min_worker_version`.
    #[serde(default)]
    pub agent_version: Option<String>,
    /// Seconds between the worker's heartbeats: the lease lasts four of them (at least 15 s, at
    /// most the configured length), so that a worker that vanishes is replaced quickly.
    #[serde(default)]
    pub heartbeat_secs: Option<u64>,
    /// The worker keeps its outputs and serves them to peers: it may take tasks whose outputs a
    /// lab node pulls (ADR-0041). Absent: false.
    #[serde(default)]
    pub serves_peers: bool,
}

impl ClaimRequest {
    /// Parsed and bounded capabilities.
    pub fn capabilities(&self) -> Result<gg_storage::queries::jobs::WorkerCapabilities, ApiError> {
        let bytes = |v: Option<u64>, what: &str| -> Result<Option<i64>, ApiError> {
            match v {
                None => Ok(None),
                Some(b) if b <= i64::MAX as u64 => Ok(Some(b as i64)),
                Some(_) => Err(ApiError::Unprocessable(format!("{what} out of range"))),
            }
        };
        let profile = match &self.profile {
            None => 1,
            Some(p) => gg_compute::graph::memory::profile_level(p)
                .filter(|n| *n <= 1000)
                .ok_or_else(|| ApiError::Unprocessable(format!("profile '{p}' is not gg-ops/N")))?
                as i32,
        };
        if self.engine.as_ref().is_some_and(|e| e.len() > 32) {
            return Err(ApiError::Unprocessable("engine too long".into()));
        }
        Ok(gg_storage::queries::jobs::WorkerCapabilities {
            ram_bytes: bytes(self.ram_bytes, "ram_bytes")?,
            device_bytes: bytes(self.device_memory_bytes, "device_memory_bytes")?,
            profile,
            engine: self.engine.clone(),
            kinds: self.kinds.clone(),
            serves_peers: self.serves_peers,
        })
    }
}

#[utoipa::path(post, path = "/worker/claim", tag = "worker", request_body = ClaimRequest, responses((status = 200, body = Claimed), (status = 204, description = "nothing to do")))]
async fn claim(
    State(state): State<AppState>,
    w: Worker,
    Json(req): Json<ClaimRequest>,
) -> Result<Response, ApiError> {
    if req.kinds.len() > 16 || req.kinds.iter().any(|k| k.len() > 64) {
        return Err(ApiError::Unprocessable("kinds list invalid".into()));
    }
    check_worker_version(
        &state.config().min_worker_version,
        req.agent_version.as_deref(),
    )?;
    let caps = req.capabilities()?;
    let lease = crate::workflows::claim::lease_for(state.config().lease_secs, req.heartbeat_secs);
    match claim_wait(&state, w.id, &req.kinds, &caps, req.wait_secs, lease).await? {
        Some(c) => {
            state
                .metrics()
                .claims_assigned
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(Json(c).into_response())
        }
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

/// A worker older than `min` gets no task: it is told to update (426), and stops.
fn check_worker_version(min: &str, agent: Option<&str>) -> Result<(), ApiError> {
    let Some(min_v) = crate::config::parse_version(min) else {
        return Ok(()); // no minimum configured
    };
    let found = agent.and_then(crate::config::parse_version);
    if found.is_some_and(|v| v >= min_v) {
        return Ok(());
    }
    let shown = agent.map(|a| a.chars().take(32).collect::<String>());
    Err(ApiError::UpgradeRequired(format!(
        "gg-worker {} is older than {min}, the oldest version this server gives tasks to: install the current release, then restart the worker",
        shown.as_deref().unwrap_or("(version not sent)")
    )))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct Heartbeat {
    pub lease_expires_at: UnixMillis,
}

#[utoipa::path(post, path = "/worker/attempts/{id}/heartbeat", tag = "worker", responses((status = 200, body = Heartbeat), (status = 409, description = "lease expired")))]
async fn heartbeat_route(
    State(state): State<AppState>,
    w: Worker,
    Path(id): Path<AttemptId>,
) -> Result<Response, ApiError> {
    // Answered only for the caller's own attempt while its lease is valid (409 otherwise).
    Ok(lease_covered(Json(Heartbeat {
        lease_expires_at: heartbeat(&state, w.id, id).await?,
    })))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct AbandonRequest {
    #[serde(default)]
    pub reason: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct Abandoned {
    pub task_state: TaskState,
}

#[utoipa::path(post, path = "/worker/attempts/{id}/abandon", tag = "worker", request_body = AbandonRequest, responses((status = 200, body = Abandoned)))]
async fn abandon_route(
    State(state): State<AppState>,
    w: Worker,
    Path(id): Path<AttemptId>,
    Json(req): Json<AbandonRequest>,
) -> Result<Json<Abandoned>, ApiError> {
    Ok(Json(Abandoned {
        task_state: abandon(&state, w.id, id, &req.reason).await?,
    }))
}

#[utoipa::path(post, path = "/worker/attempts/{id}/result", tag = "worker", request_body = Object, responses((status = 200, body = Submitted)))]
async fn result_route(
    State(state): State<AppState>,
    w: Worker,
    Path(id): Path<AttemptId>,
    Json(signed): Json<Signed<ResultSubmissionV1>>,
) -> Result<Response, ApiError> {
    let submitted =
        crate::workflows::retrying(|| submit_result(&state, w.id, w.key, id, signed.clone()))
            .await?;
    // Recording requires the caller's live lease; an identical replay is charged.
    Ok(if submitted.recorded {
        lease_covered(Json(submitted))
    } else {
        Json(submitted).into_response()
    })
}

#[utoipa::path(get, path = "/worker/receipts", tag = "worker", responses((status = 200, body = Vec<ReceiptExport>)))]
async fn receipts(
    State(state): State<AppState>,
    w: Worker,
) -> Result<Json<Vec<ReceiptExport>>, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let rows = tasks::list_receipts_of_worker(&mut c, w.id, 500).await?;
    rows.into_iter()
        .map(export_receipt)
        .collect::<Result<Vec<_>, _>>()
        .map(Json)
}

/// Whether `hash` is an input of one of the worker's active attempts.
async fn worker_may_read(
    state: &AppState,
    worker: WorkerId,
    hash: &Sha256Digest,
) -> Result<bool, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    for a in tasks::list_active_attempts_of_worker(&mut c, worker).await? {
        let Some(t) = jobs::get_task(&mut c, TaskId(a.task_id)).await? else {
            continue;
        };
        let spec = decode_spec(&t.spec_cbor)?;
        if spec.definition.inputs.values().any(|r| r.hash == *hash) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether the worker may announce itself as a holder of `hash`: an input of one of its active
/// attempts, or an output it uploaded itself. The second case is the direct hand-off of an
/// activation (ADR-0026): the producer serves the next stage, whose reader still checks every chunk
/// against the manifest the server signs from the durable copy.
async fn worker_may_announce(
    state: &AppState,
    worker: WorkerId,
    hash: &Sha256Digest,
) -> Result<bool, ApiError> {
    if worker_may_read(state, worker, hash).await? {
        return Ok(true);
    }
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if gg_storage::queries::artifacts::uploaded_by(&mut c, hash).await?
        == Some(format!("worker:{worker}"))
    {
        return Ok(true);
    }
    // Declared by this worker, not yet stored: it holds it (it may not be the only one).
    Ok(pending::declared_by(&mut c, hash, worker.0).await?)
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct AnnounceRequest {
    /// `ip:port` where this worker serves chunks of the artifact.
    pub addr: String,
    /// SHA-256 (hex) of the TLS transport key readers must pin (ADR-0013).
    pub tls_fingerprint: Sha256Digest,
}

/// What the control plane observed and recorded for an announcement.
#[derive(Serialize, utoipa::ToSchema)]
pub struct Announced {
    /// Source address the control plane saw for this call. A worker behind a NAT that preserves the
    /// port can announce this address instead of guessing its own (ADR-0013).
    pub observed_ip: String,
    /// Address actually recorded.
    pub addr: String,
    /// Seconds before the announcement expires unless refreshed.
    pub ttl_secs: i64,
}

/// Signed chunk manifest of an artifact the worker is entitled to read (ADR-0012). The digests are
/// computed by the control plane over its own bytes, so peer-supplied bytes are checked chunk by
/// chunk against a server signature, never against a peer's claim.
#[utoipa::path(get, path = "/worker/artifacts/{hash}/manifest", tag = "worker", responses((status = 200, body = Object)))]
async fn artifact_manifest(
    State(state): State<AppState>,
    w: Worker,
    Path(hash): Path<Sha256Digest>,
) -> Result<Response, ApiError> {
    if !state.config().p2p_enabled {
        return Err(ApiError::NotFound("peer distribution is disabled".into()));
    }
    if !worker_may_read(&state, w.id, &hash).await? {
        return Err(ApiError::NotFound("artifact".into()));
    }
    let manifest = crate::workflows::p2p::manifest(&state, &hash).await?;
    Ok(lease_covered(Json(
        serde_json::json!({ "manifest": manifest }),
    )))
}

/// Announce that this worker holds an artifact and serves its chunks at `addr`.
#[utoipa::path(post, path = "/worker/artifacts/{hash}/announce", tag = "worker", request_body = AnnounceRequest, responses((status = 200, body = Announced)))]
async fn artifact_announce(
    State(state): State<AppState>,
    w: Worker,
    axum::Extension(crate::client_ip::ClientIp(peer)): axum::Extension<crate::client_ip::ClientIp>,
    Path(hash): Path<Sha256Digest>,
    Json(req): Json<AnnounceRequest>,
) -> Result<Response, ApiError> {
    if !state.config().p2p_enabled {
        return Err(ApiError::NotFound("peer distribution is disabled".into()));
    }
    // An input of an active attempt is covered by its lease; an output the worker produced
    // earlier is not, and is charged.
    let covered = worker_may_read(&state, w.id, &hash).await?;
    if !covered && !worker_may_announce(&state, w.id, &hash).await? {
        return Err(ApiError::NotFound("artifact".into()));
    }
    crate::workflows::p2p::announce(&state, w.id, &hash, &req.addr, &req.tls_fingerprint).await?;
    let announced = Json(Announced {
        observed_ip: peer.to_string(),
        addr: req.addr,
        ttl_secs: state.config().p2p_peer_ttl_secs,
    });
    Ok(if covered {
        lease_covered(announced)
    } else {
        announced.into_response()
    })
}

/// Ask the holders of an artifact this worker may read, and cannot reach, to push it to this
/// worker's chunk server at `addr` (ADR-0041, A8).
#[utoipa::path(post, path = "/worker/artifacts/{hash}/reverse", tag = "worker", request_body = AnnounceRequest, responses((status = 200, body = Object)))]
async fn artifact_reverse(
    State(state): State<AppState>,
    w: Worker,
    Path(hash): Path<Sha256Digest>,
    Json(req): Json<AnnounceRequest>,
) -> Result<Response, ApiError> {
    if !worker_may_read(&state, w.id, &hash).await? {
        return Err(ApiError::NotFound("artifact".into()));
    }
    let asked = crate::workflows::push::ask(&state, &hash, &req.addr, &req.tls_fingerprint).await?;
    Ok(lease_covered(Json(
        serde_json::json!({ "holders_asked": asked }),
    )))
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct PushWait {
    /// Wait up to this long (at most 30 s) for a request.
    #[serde(default)]
    pub wait_secs: u64,
}

/// Pushes this worker is asked for: outputs it holds that a reader cannot fetch from it (A8).
#[utoipa::path(get, path = "/worker/pushes", tag = "worker", params(PushWait), responses((status = 200, body = Vec<crate::workflows::push::PushRequest>)))]
async fn pushes(
    State(state): State<AppState>,
    w: Worker,
    Query(q): Query<PushWait>,
) -> Json<Vec<crate::workflows::push::PushRequest>> {
    Json(
        state
            .push()
            .wait(crate::workflows::push::Holder::Worker(w.id), q.wait_secs)
            .await,
    )
}

/// Live holders of an artifact plus a short-lived ticket for this worker.
#[utoipa::path(get, path = "/worker/artifacts/{hash}/peers", tag = "worker", responses((status = 200, body = crate::workflows::p2p::PeersResponse)))]
async fn artifact_peers(
    State(state): State<AppState>,
    w: Worker,
    Path(hash): Path<Sha256Digest>,
) -> Result<Response, ApiError> {
    if !state.config().p2p_enabled {
        return Err(ApiError::NotFound("peer distribution is disabled".into()));
    }
    if !worker_may_read(&state, w.id, &hash).await? {
        return Err(ApiError::NotFound("artifact".into()));
    }
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let row = identities::get_worker(&mut c, w.id)
        .await?
        .ok_or_else(|| ApiError::Internal("worker vanished".into()))?;
    drop(c);
    let key: [u8; 32] = row
        .public_key
        .try_into()
        .map_err(|_| ApiError::Internal("stored worker key is not 32 bytes".into()))?;
    Ok(lease_covered(Json(
        crate::workflows::p2p::peers(&state, w.id.into(), &PublicKey(key), Some(w.id), &hash)
            .await?,
    )))
}

async fn download_input(
    State(state): State<AppState>,
    w: Worker,
    Path(hash): Path<Sha256Digest>,
    headers: axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    if !worker_may_read(&state, w.id, &hash).await? {
        return Err(ApiError::NotFound("artifact".into()));
    }
    if !state.objects().exists(&hash) {
        let mut c = state.db().pool().acquire().await?;
        if pending::get(&mut c, &hash).await?.is_some() {
            // Activation passed on before its durable copy (ADR-0026): it is held by its
            // producer; the server will have it soon.
            return Err(ApiError::Conflict(
                "artifact not yet durable: fetch it from its producer or retry".into(),
            ));
        }
        if artifacts::held_by_node(&mut c, &hash).await?.is_some() {
            drop(c);
            let range = headers
                .get("range")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().to_owned());
            // Small enough: relayed from the node through the control plane, for a worker the
            // node cannot be reached from (ADR-0041, A8).
            if let Some((node, size)) = crate::workflows::relay::relayable(&state, &hash).await? {
                return crate::workflows::relay::stream(&state, node, hash, size, range.as_deref())
                    .await
                    .map(lease_covered);
            }
            // The scientist's lab node holds it and the server keeps no copy (ADR-0041).
            return Err(ApiError::Conflict(
                "artifact held by a lab node: fetch it from peers".into(),
            ));
        }
    }
    let range = headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_owned());
    // An input of one of the caller's active attempts (`worker_may_read`).
    stream_object_ranged(&state, &hash, range.as_deref())
        .await
        .map(lease_covered)
}

async fn upload_output(
    State(state): State<AppState>,
    w: Worker,
    Path((attempt, hash)): Path<(AttemptId, Sha256Digest)>,
    Query(q): Query<UploadQuery>,
    body: Body,
) -> Result<Response, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let a = tasks::get_attempt(&mut c, attempt)
        .await?
        .ok_or_else(|| ApiError::NotFound("attempt".into()))?;
    if a.worker_id != w.id.0 {
        return Err(ApiError::NotFound("attempt".into()));
    }
    // The durable copy of a declared activation arrives after the submission (ADR-0026).
    let declared = pending::get_for_attempt(&mut c, &hash, attempt.0).await?;
    if let Some(p) = &declared
        && p.size != q.size
    {
        return Err(ApiError::Unprocessable(
            "size differs from the declared output".into(),
        ));
    }
    if declared.is_none()
        && (a
            .attempt_state()
            .map_err(|e| ApiError::Internal(e.to_string()))?
            != AttemptState::Active
            || !tasks::lease_is_valid(&mut c, attempt).await?)
    {
        return Err(ApiError::Conflict(
            "attempt is not active or its lease expired".into(),
        ));
    }
    let t = jobs::get_task(&mut c, TaskId(a.task_id))
        .await?
        .ok_or_else(|| ApiError::Internal("task missing".into()))?;
    let spec = decode_spec(&t.spec_cbor)?;
    if q.size > spec.definition.resource_limits.max_output_bytes {
        return Err(ApiError::TooLarge);
    }
    // A job a lab node holds: the control plane relays small outputs only (ADR-0041).
    if declared.is_none()
        && q.size > state.config().node_relay_max_bytes
        && jobs::output_holder(&mut c, t.job_id).await?.is_some()
    {
        return Err(ApiError::Unprocessable(format!(
            "outputs above {} bytes of a job a lab node holds are declared and pulled by the node, not uploaded",
            state.config().node_relay_max_bytes
        )));
    }
    identities::touch_worker(&mut c, w.id).await?;
    drop(c);
    let info = store_upload(
        &state,
        ProjectId(t.project_id),
        hash,
        q.size,
        &q.schema,
        "",
        &format!("worker:{}", w.id),
        body,
    )
    .await?;
    // Stored and checked: nothing waits on this hash any more. The verifications that were
    // waiting for it become claimable, and the coordinator can close a round.
    let mut c = state.db().pool().acquire().await?;
    if pending::resolve(&mut c, &hash).await? > 0 {
        state.signal_submission();
    }
    // The caller's attempt, with a valid lease or a declared output awaiting its durable copy.
    Ok(lease_covered(Json(info)))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct DeclareRequest {
    /// Size of the output, in bytes.
    pub size: u64,
    /// SHA-256 of each chunk, in order, with the chunk size the server publishes
    /// (`p2p_chunk_bytes` in `/v1/info`).
    #[schema(value_type = Vec<String>)]
    pub chunks: Vec<Sha256Digest>,
}

/// Declare an activation before its durable copy (ADR-0026): its hash, size and chunk digests. The
/// next stage then fetches it from this worker, each chunk checked against a manifest the server
/// signs over these digests, and the whole against the hash of the signed submission. Only for
/// `graph.stage.v1` tasks; the durable copy must follow (`PUT …/outputs/{hash}`) before anything is
/// verified, or the round is reissued.
#[utoipa::path(post, path = "/worker/attempts/{id}/outputs/{hash}/declare", tag = "worker", request_body = DeclareRequest, responses((status = 200, body = Object)))]
async fn declare_output(
    State(state): State<AppState>,
    w: Worker,
    Path((attempt, hash)): Path<(AttemptId, Sha256Digest)>,
    Json(req): Json<DeclareRequest>,
) -> Result<Response, ApiError> {
    if !state.config().p2p_enabled {
        return Err(ApiError::NotFound("peer distribution is disabled".into()));
    }
    let mut c = state.db().pool().acquire().await?;
    let a = tasks::get_attempt(&mut c, attempt)
        .await?
        .ok_or_else(|| ApiError::NotFound("attempt".into()))?;
    if a.worker_id != w.id.0 {
        return Err(ApiError::NotFound("attempt".into()));
    }
    if a.attempt_state()
        .map_err(|e| ApiError::Internal(e.to_string()))?
        != AttemptState::Active
        || !tasks::lease_is_valid(&mut c, attempt).await?
    {
        return Err(ApiError::Conflict(
            "attempt is not active or its lease expired".into(),
        ));
    }
    let t = jobs::get_task(&mut c, TaskId(a.task_id))
        .await?
        .ok_or_else(|| ApiError::Internal("task missing".into()))?;
    let spec = decode_spec(&t.spec_cbor)?;
    // Two cases: the activations of a staged task (ADR-0026), and the outputs of a job a lab node
    // holds, which the node pulls (ADR-0041).
    if !matches!(
        spec.definition.params,
        gg_protocol::messages::TaskParamsV1::GraphStage(_)
    ) && jobs::output_holder(&mut c, t.job_id).await?.is_none()
    {
        return Err(ApiError::Unprocessable(
            "only the activations of a staged task, or the outputs of a job a lab node holds, may be declared".into(),
        ));
    }
    if req.size > spec.definition.resource_limits.max_output_bytes {
        return Err(ApiError::TooLarge);
    }
    let chunk_size = state.config().p2p_chunk_bytes;
    if req.chunks.len() as u64 != req.size.div_ceil(chunk_size as u64) {
        return Err(ApiError::Unprocessable(format!(
            "{} chunk digests for {} bytes in chunks of {chunk_size}",
            req.chunks.len(),
            req.size
        )));
    }
    pending::declare(
        &mut c,
        &pending::NewPending {
            hash: &hash,
            attempt_id: attempt.0,
            worker_id: w.id.0,
            project_id: t.project_id,
            size: req.size,
            chunk_size,
            digests: &req.chunks,
            deadline_secs: state.config().pending_output_secs,
        },
    )
    .await?;
    // A lab node waiting for outputs to pull learns of it at once (ADR-0041).
    state.signal_node();
    Ok(lease_covered(Json(
        serde_json::json!({ "chunk_size": chunk_size }),
    )))
}

/// JSON routes.
pub fn json_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(register))
        .routes(routes!(enroll))
        .routes(routes!(challenge))
        .routes(routes!(session))
        .routes(routes!(me))
        .routes(routes!(artifact_reverse))
        .routes(routes!(pushes))
        .routes(routes!(claim))
        .routes(routes!(heartbeat_route))
        .routes(routes!(abandon_route))
        .routes(routes!(result_route))
        .routes(routes!(receipts))
        .routes(routes!(set_payout))
        .routes(routes!(artifact_manifest))
        .routes(routes!(artifact_announce))
        .routes(routes!(declare_output))
        .routes(routes!(artifact_peers))
}

/// Streaming routes.
pub fn upload_routes() -> Router<AppState> {
    Router::new()
        .route("/worker/artifacts/{hash}", get(download_input))
        .route("/worker/attempts/{id}/outputs/{hash}", put(upload_output))
}

#[cfg(test)]
mod version_tests {
    use super::check_worker_version;

    #[test]
    fn an_old_or_silent_worker_is_told_to_update() {
        assert!(
            check_worker_version("", None).is_ok(),
            "no minimum configured"
        );
        assert!(check_worker_version("0.2.0", Some("0.2.0")).is_ok());
        assert!(check_worker_version("0.2.0", Some("0.10.1")).is_ok());
        assert!(check_worker_version("0.2.0", Some("1.0.0-rc.1")).is_ok());
        for old in [Some("0.1.9"), Some("garbage"), None] {
            let e = check_worker_version("0.2.0", old).unwrap_err().to_string();
            assert!(
                e.contains("older than 0.2.0") && e.contains("install the current release"),
                "{e}"
            );
        }
    }
}
