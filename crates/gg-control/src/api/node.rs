//! Lab node routes (ADR-0041): a researcher registers the key of its machine's node; the node signs
//! in by challenge like a worker, declares the artifacts it holds (the server records their hash,
//! size and chunk digests and keeps no copy), and announces where it serves them. It also pulls
//! the outputs of the jobs it holds from the workers that produced them (A4).

use axum::Json;
use axum::extract::{Path, State};
use gg_core::*;
use gg_protocol::keys::PublicKey;
use gg_protocol::messages::LoginChallengeV1;
use gg_protocol::signed::Signed;
use gg_storage::queries::{artifacts, misc, nodes, p2p, pending};
use serde::{Deserialize, Serialize};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::api::worker::{AnnounceRequest, Announced};
use crate::auth::{LabNode, Researcher, require_project_role};
use crate::error::ApiError;
use crate::state::AppState;
use crate::workflows::login::{Challenge, Session, create_session, issue_challenge};

/// Longest display name of a node.
const MAX_NAME: usize = 64;

#[derive(Deserialize, utoipa::ToSchema)]
pub struct RegisterNode {
    /// Ed25519 public key of the node (hex).
    pub public_key: String,
    /// Shown to the researcher (a host name, for instance).
    #[serde(default)]
    pub name: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct NodeView {
    pub node_id: NodeId,
    pub name: String,
    /// Ed25519 public key (hex).
    pub public_key: String,
    pub researcher_id: ResearcherId,
    pub revoked: bool,
    #[schema(value_type = Option<String>)]
    pub last_seen_at: Option<String>,
}

fn view(n: gg_storage::models::LabNodeRow) -> NodeView {
    NodeView {
        node_id: NodeId(n.id),
        name: n.display_name,
        public_key: hex::encode(&n.public_key),
        researcher_id: ResearcherId(n.researcher_id),
        revoked: n.revoked_at.is_some(),
        last_seen_at: n.last_seen_at.map(|t| t.to_string()),
    }
}

/// Register the key of a node that will hold and serve this researcher's runs.
#[utoipa::path(post, path = "/researcher/nodes", tag = "researcher", request_body = RegisterNode, responses((status = 200, body = NodeView)))]
async fn register(
    State(state): State<AppState>,
    Researcher(rid): Researcher,
    Json(req): Json<RegisterNode>,
) -> Result<Json<NodeView>, ApiError> {
    let key: PublicKey = req.public_key.parse()?;
    if req.name.len() > MAX_NAME || req.name.chars().any(char::is_control) {
        return Err(ApiError::Unprocessable(format!(
            "name: at most {MAX_NAME} printable characters"
        )));
    }
    let mut tx = state.db().begin().await?;
    if nodes::get_by_key(&mut tx, &key.0).await?.is_some() {
        return Err(ApiError::Conflict("this key is already registered".into()));
    }
    let id = NodeId::new();
    nodes::insert(&mut tx, id, rid, &key.0, &req.name).await?;
    misc::audit(
        &mut tx,
        &format!("researcher:{rid}"),
        "node.register",
        &id.to_string(),
        serde_json::json!({ "name": req.name }),
    )
    .await?;
    let row = nodes::get(&mut tx, id)
        .await?
        .ok_or_else(|| ApiError::Internal("node just inserted".into()))?;
    tx.commit().await?;
    Ok(Json(view(row)))
}

/// The nodes the researcher owns or was granted the use of.
#[utoipa::path(get, path = "/researcher/nodes", tag = "researcher", responses((status = 200, body = Vec<NodeView>)))]
async fn list(
    State(state): State<AppState>,
    Researcher(rid): Researcher,
) -> Result<Json<Vec<NodeView>>, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(
        nodes::usable_by(&mut c, rid)
            .await?
            .into_iter()
            .map(view)
            .collect(),
    ))
}

/// Revoke one of the researcher's nodes: its sessions end at once.
#[utoipa::path(post, path = "/researcher/nodes/{id}/revoke", tag = "researcher", responses((status = 200, body = Object)))]
async fn revoke(
    State(state): State<AppState>,
    Researcher(rid): Researcher,
    Path(id): Path<NodeId>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut tx = state.db().begin().await?;
    let revoked = nodes::revoke(&mut tx, rid, id).await?;
    if revoked {
        misc::audit(
            &mut tx,
            &format!("researcher:{rid}"),
            "node.revoke",
            &id.to_string(),
            serde_json::json!({}),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Json(serde_json::json!({ "revoked": revoked })))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct GrantRequest {
    /// The researcher allowed to attach this node to their projects.
    pub researcher_id: ResearcherId,
}

/// The node, if `rid` owns it.
async fn owned(
    state: &AppState,
    rid: ResearcherId,
    id: NodeId,
) -> Result<gg_storage::models::LabNodeRow, ApiError> {
    let mut c = state.db().pool().acquire().await?;
    nodes::get(&mut c, id)
        .await?
        .filter(|n| n.researcher_id == rid.0)
        .ok_or_else(|| ApiError::NotFound("node".into()))
}

/// Allow another researcher to use this node: to attach it to their projects (ADR-0041, A7). Its
/// owner only.
#[utoipa::path(post, path = "/researcher/nodes/{id}/users", tag = "researcher", request_body = GrantRequest, responses((status = 200, body = Object)))]
async fn grant(
    State(state): State<AppState>,
    Researcher(rid): Researcher,
    Path(id): Path<NodeId>,
    Json(req): Json<GrantRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let node = owned(&state, rid, id).await?;
    if node.revoked_at.is_some() {
        return Err(ApiError::Conflict("this node is revoked".into()));
    }
    let mut tx = state.db().begin().await?;
    if gg_storage::queries::identities::get_researcher(&mut tx, req.researcher_id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound("researcher".into()));
    }
    nodes::grant(&mut tx, id, req.researcher_id).await?;
    misc::audit(
        &mut tx,
        &format!("researcher:{rid}"),
        "node.grant",
        &id.to_string(),
        serde_json::json!({ "researcher_id": req.researcher_id }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(serde_json::json!({ "granted": true })))
}

/// Withdraw a researcher's use of this node. The projects they attached stay attached until a
/// member of each detaches it. Its owner only.
#[utoipa::path(post, path = "/researcher/nodes/{id}/users/{researcher}/remove", tag = "researcher", responses((status = 200, body = Object)))]
async fn ungrant(
    State(state): State<AppState>,
    Researcher(rid): Researcher,
    Path((id, researcher)): Path<(NodeId, ResearcherId)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    owned(&state, rid, id).await?;
    let mut tx = state.db().begin().await?;
    let removed = nodes::ungrant(&mut tx, id, researcher).await?;
    if removed {
        misc::audit(
            &mut tx,
            &format!("researcher:{rid}"),
            "node.ungrant",
            &id.to_string(),
            serde_json::json!({ "researcher_id": researcher }),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Json(serde_json::json!({ "removed": removed })))
}

/// Researchers granted the use of this node. Its owner only.
#[utoipa::path(get, path = "/researcher/nodes/{id}/users", tag = "researcher", responses((status = 200, body = Vec<ResearcherId>)))]
async fn users(
    State(state): State<AppState>,
    Researcher(rid): Researcher,
    Path(id): Path<NodeId>,
) -> Result<Json<Vec<ResearcherId>>, ApiError> {
    owned(&state, rid, id).await?;
    let mut c = state.db().pool().acquire().await?;
    Ok(Json(nodes::users(&mut c, id).await?))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct AttachRequest {
    pub node_id: NodeId,
}

/// Attach a node to a project: it may then hold the project's artifacts, pull its jobs' outputs
/// and step its runs. A project member, allowed to use the node.
#[utoipa::path(post, path = "/researcher/projects/{id}/nodes", tag = "researcher", request_body = AttachRequest, responses((status = 200, body = Object)))]
async fn attach(
    State(state): State<AppState>,
    Researcher(rid): Researcher,
    Path(project): Path<ProjectId>,
    Json(req): Json<AttachRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_project_role(&state, rid, project, ProjectRole::Member).await?;
    let mut tx = state.db().begin().await?;
    let node = nodes::get(&mut tx, req.node_id)
        .await?
        .filter(|n| n.revoked_at.is_none());
    // An unknown node and one the researcher may not use look the same.
    if node.is_none() || !nodes::may_use(&mut tx, req.node_id, rid).await? {
        return Err(ApiError::NotFound("node".into()));
    }
    let attached = nodes::attach(&mut tx, req.node_id, project, rid).await?;
    if attached {
        misc::audit(
            &mut tx,
            &format!("researcher:{rid}"),
            "node.attach",
            &req.node_id.to_string(),
            serde_json::json!({ "project_id": project }),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Json(serde_json::json!({ "attached": true })))
}

/// Detach a node from a project, once it no longer steps a run or pulls a job's outputs there. A
/// project member.
#[utoipa::path(post, path = "/researcher/projects/{id}/nodes/{node}/detach", tag = "researcher", responses((status = 200, body = Object)))]
async fn detach(
    State(state): State<AppState>,
    Researcher(rid): Researcher,
    Path((project, node)): Path<(ProjectId, NodeId)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_project_role(&state, rid, project, ProjectRole::Member).await?;
    let mut tx = state.db().begin().await?;
    let busy = nodes::busy_for_project(&mut tx, node, project).await?;
    if busy > 0 {
        return Err(ApiError::Conflict(format!(
            "the node still works for this project ({busy} running runs or unfinished jobs): cancel them first"
        )));
    }
    let detached = nodes::detach(&mut tx, node, project).await?;
    if detached {
        misc::audit(
            &mut tx,
            &format!("researcher:{rid}"),
            "node.detach",
            &node.to_string(),
            serde_json::json!({ "project_id": project }),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Json(serde_json::json!({ "detached": detached })))
}

/// Nodes attached to a project.
#[utoipa::path(get, path = "/researcher/projects/{id}/nodes", tag = "researcher", responses((status = 200, body = Vec<NodeView>)))]
async fn project_nodes(
    State(state): State<AppState>,
    Researcher(rid): Researcher,
    Path(project): Path<ProjectId>,
) -> Result<Json<Vec<NodeView>>, ApiError> {
    require_project_role(&state, rid, project, ProjectRole::Viewer).await?;
    let mut c = state.db().pool().acquire().await?;
    Ok(Json(
        nodes::nodes_of_project(&mut c, project)
            .await?
            .into_iter()
            .map(view)
            .collect(),
    ))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ChallengeRequest {
    /// Ed25519 public key (hex).
    pub public_key: String,
}

#[utoipa::path(post, path = "/node/challenge", tag = "node", request_body = ChallengeRequest, responses((status = 200, body = Challenge)))]
async fn challenge(
    State(state): State<AppState>,
    Json(req): Json<ChallengeRequest>,
) -> Result<Json<Challenge>, ApiError> {
    let key: PublicKey = req.public_key.parse()?;
    Ok(Json(issue_challenge(&state, key, "node-login").await?))
}

#[utoipa::path(post, path = "/node/session", tag = "node", request_body = Object, responses((status = 200, body = Session)))]
async fn session(
    State(state): State<AppState>,
    Json(signed): Json<Signed<LoginChallengeV1>>,
) -> Result<Json<Session>, ApiError> {
    if signed.body.action != "node-login" {
        return Err(ApiError::Unprocessable("action must be node-login".into()));
    }
    Ok(Json(create_session(&state, signed).await?))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct NodeMe {
    pub node_id: NodeId,
    /// The researcher who registered it (its owner).
    pub researcher_id: ResearcherId,
    /// The projects it works for.
    pub projects: Vec<ProjectId>,
    /// The source address this request came from, as the control plane sees it: behind a NAT
    /// that preserves the port, the address to announce (A8).
    pub observed_ip: String,
}

/// Who this node is, and for which projects it works.
#[utoipa::path(get, path = "/node/me", tag = "node", responses((status = 200, body = NodeMe)))]
async fn me(
    State(state): State<AppState>,
    n: LabNode,
    axum::Extension(crate::client_ip::ClientIp(peer)): axum::Extension<crate::client_ip::ClientIp>,
) -> Result<Json<NodeMe>, ApiError> {
    let mut c = state.db().pool().acquire().await?;
    Ok(Json(NodeMe {
        node_id: n.id,
        researcher_id: n.researcher,
        projects: nodes::projects_of(&mut c, n.id).await?,
        observed_ip: peer.to_string(),
    }))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct HoldRequest {
    /// Project the artifact belongs to; the node's researcher must be a member.
    pub project_id: ProjectId,
    /// Size in bytes.
    pub size: u64,
    /// SHA-256 of each chunk, in order, in chunks of the server's `p2p_chunk_bytes`.
    pub chunks: Vec<Sha256Digest>,
    /// Schema label (e.g. `ggt1-f32`).
    #[serde(default)]
    pub schema: String,
    /// The first bytes of the artifact (hex, at most 64 bytes): what the server reads of it
    /// without holding it, such as a dataset's shape in its tensor header.
    #[serde(default)]
    pub header: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct Held {
    pub hash: Sha256Digest,
    pub size: u64,
    /// Chunk size of the manifest readers will check against.
    pub chunk_size: u32,
    /// Whether the server already stores these bytes itself (an upload): the node need not serve
    /// them, and nothing was recorded.
    pub server_holds: bool,
}

/// Declare an artifact this node holds (ADR-0041). The server records its hash, size and chunk
/// digests and signs their manifest for authorized readers; it keeps no copy. The digests are the
/// node's: a reader checks every chunk against them and the whole against the hash, so a node that
/// declares wrong ones only stops its own researcher's runs.
#[utoipa::path(post, path = "/node/artifacts/{hash}", tag = "node", request_body = HoldRequest, responses((status = 200, body = Held)))]
async fn hold(
    State(state): State<AppState>,
    n: LabNode,
    Path(hash): Path<Sha256Digest>,
    Json(req): Json<HoldRequest>,
) -> Result<Json<Held>, ApiError> {
    if !state.config().p2p_enabled {
        return Err(ApiError::NotFound("peer distribution is disabled".into()));
    }
    {
        // The node holds artifacts only for the projects attached to it.
        let mut c = state.db().pool().acquire().await?;
        if !nodes::attached(&mut c, n.id, req.project_id).await? {
            return Err(ApiError::NotFound("project".into()));
        }
    }
    if req.schema.len() > 64 || req.schema.chars().any(char::is_control) {
        return Err(ApiError::Unprocessable("schema label too long".into()));
    }
    let chunk_size = state.config().p2p_chunk_bytes;
    // The same structural checks a reader applies to the signed manifest: chunk count matching
    // the size, and bounded.
    gg_protocol::messages::ChunkManifestV1 {
        protocol_version: gg_protocol::PROTOCOL_VERSION,
        network: state.network().clone(),
        artifact_hash: hash,
        size: req.size,
        chunk_size,
        chunks: req.chunks.clone(),
        issued_at: UnixMillis::now(),
    }
    .validate_structure()
    .map_err(|e| ApiError::Unprocessable(format!("chunks: {e}")))?;
    let header =
        hex::decode(&req.header).map_err(|_| ApiError::Unprocessable("header: not hex".into()))?;
    if header.len() > 64 || header.len() as u64 > req.size {
        return Err(ApiError::Unprocessable(
            "header: at most 64 bytes, within the artifact".into(),
        ));
    }
    let held = Held {
        hash,
        size: req.size,
        chunk_size,
        server_holds: false,
    };
    let mut tx = state.db().begin().await?;
    if artifacts::is_withdrawn(&mut tx, &hash).await? {
        return Err(ApiError::Conflict(
            "this artifact was withdrawn by an operator".into(),
        ));
    }
    if let Some(row) = artifacts::get_artifact(&mut tx, &hash).await? {
        if row.size as u64 != req.size {
            return Err(ApiError::Conflict(
                "a different size is recorded for this hash".into(),
            ));
        }
        if state.objects().exists(&hash) {
            return Ok(Json(Held {
                server_holds: true,
                ..held
            }));
        }
        // Declared again by the node that holds it (a restart): the same digests, or a refusal.
        let same = artifacts::held_by_node(&mut tx, &hash).await? == Some(n.id)
            && p2p::get_chunks(&mut tx, &hash)
                .await?
                .is_some_and(|c| c.chunk_size == chunk_size && c.digests == req.chunks);
        if !same {
            return Err(ApiError::Conflict(
                "this artifact is already recorded with another holder or other chunks".into(),
            ));
        }
        if !header.is_empty() {
            artifacts::set_node_header(&mut tx, &hash, &header).await?;
            tx.commit().await?;
        }
        return Ok(Json(held));
    }
    artifacts::insert_node_artifact(&mut tx, &hash, req.size, &req.schema, req.project_id, n.id)
        .await?;
    p2p::insert_chunks(&mut tx, &hash, req.size, chunk_size, &req.chunks).await?;
    if !header.is_empty() {
        artifacts::set_node_header(&mut tx, &hash, &header).await?;
    }
    misc::audit(
        &mut tx,
        &format!("node:{}", n.id),
        "node.hold",
        &hash.to_hex(),
        serde_json::json!({ "project_id": req.project_id, "size": req.size }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(held))
}

/// Announce where this node serves the chunks of an artifact it holds.
#[utoipa::path(post, path = "/node/artifacts/{hash}/announce", tag = "node", request_body = AnnounceRequest, responses((status = 200, body = Announced)))]
async fn announce(
    State(state): State<AppState>,
    n: LabNode,
    axum::Extension(crate::client_ip::ClientIp(peer)): axum::Extension<crate::client_ip::ClientIp>,
    Path(hash): Path<Sha256Digest>,
    Json(req): Json<AnnounceRequest>,
) -> Result<Json<Announced>, ApiError> {
    if !state.config().p2p_enabled {
        return Err(ApiError::NotFound("peer distribution is disabled".into()));
    }
    {
        let mut c = state.db().pool().acquire().await?;
        if artifacts::held_by_node(&mut c, &hash).await? != Some(n.id) {
            return Err(ApiError::NotFound("artifact".into()));
        }
    }
    crate::workflows::p2p::announce_node(&state, n.id, &hash, &req.addr, &req.tls_fingerprint)
        .await?;
    Ok(Json(Announced {
        observed_ip: peer.to_string(),
        addr: req.addr,
        ttl_secs: state.config().p2p_peer_ttl_secs,
    }))
}

/// Whether `node` has a reason to read `hash`: an output of a job it holds, still to pull; an
/// artifact it holds itself; or an artifact of a project it is attached to (the node works with
/// that project's data: its description, the small outputs the server relayed).
async fn node_may_read(
    state: &AppState,
    node: NodeId,
    hash: &Sha256Digest,
) -> Result<bool, ApiError> {
    let mut c = state.db().pool().acquire().await?;
    if pending::is_for_node(&mut c, hash, node.0).await?
        || artifacts::held_by_node(&mut c, hash).await? == Some(node)
        || gg_storage::queries::collective::node_run_names(&mut c, node.0, hash).await?
    {
        return Ok(true);
    }
    let Some(row) = artifacts::get_artifact(&mut c, hash).await? else {
        return Ok(false);
    };
    Ok(match row.project_id {
        Some(p) => nodes::attached(&mut c, node, ProjectId(p)).await?,
        None => false,
    })
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct PullItem {
    pub hash: Sha256Digest,
    pub size: u64,
    pub project_id: ProjectId,
    pub attempt_id: AttemptId,
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct WaitQuery {
    /// Wait up to this long (at most 30 s) for something to come, instead of answering empty.
    #[serde(default)]
    pub wait_secs: u64,
}

/// Answer `check` as soon as it has something, waiting up to `wait_secs` for the node signal.
async fn long_poll<T, F, Fut>(
    state: &AppState,
    wait_secs: u64,
    check: F,
) -> Result<Vec<T>, ApiError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<T>, ApiError>>,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait_secs.min(30));
    loop {
        let found = crate::state::Signals::wait_or(
            &state.signals().node,
            deadline.saturating_duration_since(std::time::Instant::now()),
            || async { Ok(Some(check().await?).filter(|v: &Vec<T>| !v.is_empty())) },
        )
        .await?;
        if let Some(v) = found {
            return Ok(v);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(Vec::new());
        }
    }
}

/// Outputs of the jobs this node holds that wait for it, oldest first (at most 64).
#[utoipa::path(get, path = "/node/pulls", tag = "node", params(WaitQuery), responses((status = 200, body = Vec<PullItem>)))]
async fn pulls(
    State(state): State<AppState>,
    n: LabNode,
    axum::extract::Query(q): axum::extract::Query<WaitQuery>,
) -> Result<Json<Vec<PullItem>>, ApiError> {
    {
        let mut c = state.db().pool().acquire().await?;
        nodes::touch(&mut c, n.id).await?;
    }
    Ok(Json(
        long_poll(&state, q.wait_secs, || async {
            let mut c = state.db().pool().acquire().await?;
            Ok(pending::to_pull(&mut c, n.id.0, 64)
                .await?
                .into_iter()
                .map(|p| PullItem {
                    hash: p.hash,
                    size: p.size,
                    project_id: ProjectId(p.project_id),
                    attempt_id: AttemptId(p.attempt_id),
                })
                .collect())
        })
        .await?,
    ))
}

/// Signed chunk manifest of an artifact this node may read.
#[utoipa::path(get, path = "/node/artifacts/{hash}/manifest", tag = "node", responses((status = 200, body = Object)))]
async fn manifest(
    State(state): State<AppState>,
    n: LabNode,
    Path(hash): Path<Sha256Digest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !state.config().p2p_enabled || !node_may_read(&state, n.id, &hash).await? {
        return Err(ApiError::NotFound("artifact".into()));
    }
    let manifest = crate::workflows::p2p::manifest(&state, &hash).await?;
    Ok(Json(serde_json::json!({ "manifest": manifest })))
}

/// Holders of an artifact this node may read, and a ticket for the node.
#[utoipa::path(get, path = "/node/artifacts/{hash}/peers", tag = "node", responses((status = 200, body = crate::workflows::p2p::PeersResponse)))]
async fn peers(
    State(state): State<AppState>,
    n: LabNode,
    Path(hash): Path<Sha256Digest>,
) -> Result<Json<crate::workflows::p2p::PeersResponse>, ApiError> {
    if !state.config().p2p_enabled || !node_may_read(&state, n.id, &hash).await? {
        return Err(ApiError::NotFound("artifact".into()));
    }
    Ok(Json(
        crate::workflows::p2p::peers(&state, n.id.into(), &n.key, None, &hash).await?,
    ))
}

/// Bytes of an artifact this node may read, when the control plane stores it (a small output a
/// worker uploaded); otherwise `409`: it comes from peers.
async fn download(
    State(state): State<AppState>,
    n: LabNode,
    Path(hash): Path<Sha256Digest>,
    headers: axum::http::HeaderMap,
) -> Result<axum::response::Response, ApiError> {
    if !node_may_read(&state, n.id, &hash).await? {
        return Err(ApiError::NotFound("artifact".into()));
    }
    if !state.objects().exists(&hash) {
        return Err(ApiError::Conflict(
            "not stored by the control plane: fetch it from peers".into(),
        ));
    }
    let range = headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_owned());
    crate::api::researcher::stream_object_ranged(&state, &hash, range.as_deref()).await
}

/// The node has pulled an output and checked it against its hash: it becomes its holder, and the
/// output is no longer pending. The verifications waiting for it become claimable.
#[utoipa::path(post, path = "/node/artifacts/{hash}/pulled", tag = "node", responses((status = 200, body = Held)))]
async fn pulled(
    State(state): State<AppState>,
    n: LabNode,
    Path(hash): Path<Sha256Digest>,
) -> Result<Json<Held>, ApiError> {
    let mut tx = state.db().begin().await?;
    if !pending::is_for_node(&mut tx, &hash, n.id.0).await? {
        return Err(ApiError::NotFound(
            "no output of this node's jobs waits under this hash".into(),
        ));
    }
    let p = pending::get(&mut tx, &hash)
        .await?
        .ok_or_else(|| ApiError::Internal("pending output vanished".into()))?;
    let inserted =
        artifacts::insert_node_artifact(&mut tx, &hash, p.size, "", ProjectId(p.project_id), n.id)
            .await?;
    if !inserted
        && !state.objects().exists(&hash)
        && artifacts::held_by_node(&mut tx, &hash).await? != Some(n.id)
    {
        return Err(ApiError::Conflict(
            "this hash is already recorded with another holder".into(),
        ));
    }
    p2p::insert_chunks(&mut tx, &hash, p.size, p.chunk_size, &p.digests).await?;
    pending::resolve(&mut tx, &hash).await?;
    tx.commit().await?;
    state.signal_submission();
    Ok(Json(Held {
        hash,
        size: p.size,
        chunk_size: p.chunk_size,
        server_holds: state.objects().exists(&hash),
    }))
}

/// The node gives up pulling an output: its producer does not serve it, or serves bytes that do
/// not match what it declared. The attempts that submitted it are closed as inconclusive (not paid,
/// nobody sanctioned) and their tasks sent again, as when the deadline passes: this only brings
/// that moment forward, which the node could force anyway by staying offline.
#[utoipa::path(post, path = "/node/artifacts/{hash}/unavailable", tag = "node", responses((status = 200, body = Object)))]
async fn unavailable(
    State(state): State<AppState>,
    n: LabNode,
    Path(hash): Path<Sha256Digest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let attempts = {
        let mut c = state.db().pool().acquire().await?;
        pending::submitted_attempts_for_node(&mut c, &hash, n.id.0).await?
    };
    let mut reissued = 0;
    for a in attempts {
        if crate::workflows::reaper::reissue_unpulled_attempt(&state, AttemptId(a), "node gave up")
            .await?
        {
            reissued += 1;
        }
    }
    Ok(Json(serde_json::json!({ "reissued": reissued })))
}

/// Steps due on the training runs this node holds (ADR-0041, A6).
#[utoipa::path(get, path = "/node/steps", tag = "node", params(WaitQuery), responses((status = 200, body = Vec<crate::workflows::node_step::StepWork>)))]
async fn steps(
    State(state): State<AppState>,
    n: LabNode,
    axum::extract::Query(q): axum::extract::Query<WaitQuery>,
) -> Result<Json<Vec<crate::workflows::node_step::StepWork>>, ApiError> {
    Ok(Json(
        long_poll(&state, q.wait_secs, || {
            crate::workflows::node_step::due(&state, n.id)
        })
        .await?,
    ))
}

/// The node reports a step it applied: the server checks the checkpoint against the run's plan,
/// records it and opens the next round.
#[utoipa::path(post, path = "/node/runs/{id}/steps", tag = "node", request_body = crate::workflows::node_step::StepReport, responses((status = 200, body = Object)))]
async fn report_step(
    State(state): State<AppState>,
    n: LabNode,
    Path(id): Path<uuid::Uuid>,
    Json(report): Json<crate::workflows::node_step::StepReport>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let recorded = crate::workflows::node_step::accept(&state, n.id, id, report).await?;
    Ok(Json(serde_json::json!({ "recorded": recorded })))
}

/// Comparisons this node is asked for (ADR-0042): two computations of a task of a job it holds,
/// whose outputs it has pulled. Waits up to `wait_secs` for some.
#[utoipa::path(get, path = "/node/comparisons", tag = "node", params(WaitQuery), responses((status = 200, body = Vec<crate::workflows::replicas::ComparisonWork>)))]
async fn comparisons(
    State(state): State<AppState>,
    n: LabNode,
    axum::extract::Query(q): axum::extract::Query<WaitQuery>,
) -> Result<Json<Vec<crate::workflows::replicas::ComparisonWork>>, ApiError> {
    Ok(Json(
        long_poll(&state, q.wait_secs, || {
            crate::workflows::replicas::node_work(&state, n.id)
        })
        .await?,
    ))
}

/// A comparison the node signed.
#[utoipa::path(post, path = "/node/comparisons", tag = "node", request_body = Object, responses((status = 200, body = Object)))]
async fn report_comparison(
    State(state): State<AppState>,
    n: LabNode,
    Json(signed): Json<Signed<gg_protocol::messages::ComparisonV1>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let recorded =
        crate::workflows::replicas::accept_node_comparison(&state, n.id, &n.key, signed).await?;
    Ok(Json(serde_json::json!({ "recorded": recorded })))
}

/// Micro-batch rows this node is to cut for the runs it steps: for the open round and the next.
#[utoipa::path(get, path = "/node/rows", tag = "node", responses((status = 200, body = Vec<crate::workflows::node_step::RowsDue>)))]
async fn rows_due(
    State(state): State<AppState>,
    n: LabNode,
) -> Result<Json<Vec<crate::workflows::node_step::RowsDue>>, ApiError> {
    Ok(Json(
        crate::workflows::node_step::rows_due(&state, n.id).await?,
    ))
}

/// The node declares the rows it cut for a step: the server checks their sizes and holder.
#[utoipa::path(post, path = "/node/runs/{id}/rows", tag = "node", request_body = crate::workflows::node_step::RowsReport, responses((status = 204)))]
async fn declare_rows(
    State(state): State<AppState>,
    n: LabNode,
    Path(id): Path<uuid::Uuid>,
    Json(report): Json<crate::workflows::node_step::RowsReport>,
) -> Result<axum::http::StatusCode, ApiError> {
    crate::workflows::node_step::accept_rows(&state, n.id, id, report).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Chunks this node is asked to relay through the control plane (A8), waiting up to `wait_secs`
/// for some.
#[utoipa::path(get, path = "/node/relay", tag = "node", params(WaitQuery), responses((status = 200, body = Vec<crate::workflows::relay::RelayItem>)))]
async fn relay_requests(
    State(state): State<AppState>,
    n: LabNode,
    axum::extract::Query(q): axum::extract::Query<WaitQuery>,
) -> Result<Json<Vec<crate::workflows::relay::RelayItem>>, ApiError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(q.wait_secs.min(30));
    loop {
        let notified = state.relay().queued.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let items = state.relay().take(n.id, 64);
        if !items.is_empty() {
            return Ok(Json(items));
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Ok(Json(Vec::new()));
        }
        let _ = tokio::time::timeout(left, notified).await;
    }
}

/// One chunk the node relays: handed to the reader waiting for it. The reader checks it against
/// the signed manifest; the server checks only its size.
async fn relay_chunk(
    State(state): State<AppState>,
    n: LabNode,
    Path(id): Path<uuid::Uuid>,
    body: axum::body::Bytes,
) -> Result<axum::response::Response, ApiError> {
    let taken = state.relay().deliver(n.id, id, body.to_vec());
    if !taken {
        return Err(ApiError::NotFound(
            "no reader waits for this chunk any more".into(),
        ));
    }
    // Asked for by a reader, which paid for its own request: the node is not charged for it.
    Ok(crate::quota::lease_covered(Json(
        serde_json::json!({ "taken": true }),
    )))
}

/// Ask the holders of an artifact this node may read, and cannot reach (a worker's output), to push
/// it to this node's chunk server at `addr` (A8).
#[utoipa::path(post, path = "/node/artifacts/{hash}/reverse", tag = "node", request_body = AnnounceRequest, responses((status = 200, body = Object)))]
async fn reverse(
    State(state): State<AppState>,
    n: LabNode,
    Path(hash): Path<Sha256Digest>,
    Json(req): Json<AnnounceRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !node_may_read(&state, n.id, &hash).await? {
        return Err(ApiError::NotFound("artifact".into()));
    }
    let asked = crate::workflows::push::ask(&state, &hash, &req.addr, &req.tls_fingerprint).await?;
    Ok(Json(serde_json::json!({ "holders_asked": asked })))
}

/// Pushes this node is asked for: artifacts it holds that a reader cannot fetch from it (A8).
#[utoipa::path(get, path = "/node/pushes", tag = "node", params(WaitQuery), responses((status = 200, body = Vec<crate::workflows::push::PushRequest>)))]
async fn pushes(
    State(state): State<AppState>,
    n: LabNode,
    axum::extract::Query(q): axum::extract::Query<WaitQuery>,
) -> Json<Vec<crate::workflows::push::PushRequest>> {
    Json(
        state
            .push()
            .wait(crate::workflows::push::Holder::Node(n.id), q.wait_secs)
            .await,
    )
}

/// Streaming routes.
pub fn stream_routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/node/artifacts/{hash}", axum::routing::get(download))
        // A chunk is at most 16 MiB (the protocol's largest chunk size).
        .route(
            "/node/relay/{id}",
            axum::routing::put(relay_chunk).layer(axum::extract::DefaultBodyLimit::max(16 << 20)),
        )
}

/// JSON routes.
pub fn json_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(register, list))
        .routes(routes!(revoke))
        .routes(routes!(grant, users))
        .routes(routes!(ungrant))
        .routes(routes!(attach, project_nodes))
        .routes(routes!(detach))
        .routes(routes!(challenge))
        .routes(routes!(session))
        .routes(routes!(me))
        .routes(routes!(hold))
        .routes(routes!(announce))
        .routes(routes!(pulls))
        .routes(routes!(manifest))
        .routes(routes!(peers))
        .routes(routes!(pulled))
        .routes(routes!(unavailable))
        .routes(routes!(steps))
        .routes(routes!(report_step))
        .routes(routes!(comparisons, report_comparison))
        .routes(routes!(rows_due))
        .routes(routes!(declare_rows))
        .routes(routes!(relay_requests))
        .routes(routes!(reverse))
        .routes(routes!(pushes))
}
