//! The node's local API (ADR-0041, A7): what the Python library talks to. It listens on this
//! machine by default, on the laboratory's network over TLS when configured so.
//!
//! Every request carries the researcher's GPU Goblin token. The node checks it with the control
//! plane (`/researcher/me`) and acts for that researcher only: the control plane decides what the
//! researcher may do (project membership, run access), the node adds what only it knows (what it
//! holds, what it is pulling). The token is only ever sent to the control plane it belongs to.
//!
//! - `GET  /v1/status`: servers, projects the node works for, what it holds and pulls;
//! - `PUT  /v1/projects/{project}/data?schema=…`: a dataset, kept here and declared (never sent to
//!   the control plane);
//! - `POST /v1/projects/{project}/runs`: a training run the node steps (`holder_node` set here);
//! - `GET  /v1/runs/{run}`: the run, and the tasks of its current round by state;
//! - `GET  /v1/runs/{run}/checkpoint/{name}`: a part of the run's current checkpoint, from here;
//! - `POST /v1/runs/{run}/cancel`.
//!
//! With several servers, a request names its network (`X-GG-Network`); with one, it need not.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use gg_client::client::ClientError;
use gg_core::{ProjectId, Sha256Digest};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Node;
use crate::service::Shared;

/// An error answered to the library, with the status the control plane gave when it gave one.
pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "message": self.1 }))).into_response()
    }
}

impl From<ClientError> for ApiError {
    fn from(e: ClientError) -> Self {
        match e {
            ClientError::Status { status, message } => ApiError(
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                message,
            ),
            other => ApiError(StatusCode::BAD_GATEWAY, other.to_string()),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

fn err(status: StatusCode, m: impl Into<String>) -> ApiError {
    ApiError(status, m.into())
}

/// State of the local API.
struct Api {
    shared: Arc<Shared>,
    /// Tokens checked recently: SHA-256 of the token → (researcher, until).
    checked: std::sync::Mutex<BTreeMap<Sha256Digest, (String, Instant)>>,
}

/// How long a token checked with the control plane is trusted without asking again.
const TOKEN_CACHE: Duration = Duration::from_secs(60);

/// The caller: its token, the node of the server it belongs to, and its researcher id.
struct Caller {
    token: String,
    node: Arc<Node>,
}

impl Api {
    async fn caller(&self, headers: &HeaderMap) -> Result<Caller, ApiError> {
        let token = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "a researcher token is required"))?
            .to_owned();
        let network = headers
            .get("x-gg-network")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let node = {
            let served = self.shared.served.read().await;
            let pick: Vec<&crate::service::Served> = served
                .iter()
                .filter(|s| network.as_ref().is_none_or(|n| &s.entry.network == n))
                .collect();
            match pick.as_slice() {
                [one] => one.node.clone(),
                [] => {
                    return Err(err(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "this node works with no such server yet (see /v1/status)",
                    ));
                }
                _ => {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "this node works with several servers: name one with X-GG-Network",
                    ));
                }
            }
        };
        let digest = Sha256Digest::of_bytes(token.as_bytes());
        let fresh = self
            .checked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&digest)
            .is_some_and(|(_, until)| Instant::now() < *until);
        if !fresh {
            let me: Value = node.api.get_json(&token, "/researcher/me").await?;
            let id = me["researcher_id"].as_str().unwrap_or_default().to_owned();
            if id.is_empty() {
                return Err(err(StatusCode::UNAUTHORIZED, "not a researcher token"));
            }
            let mut c = self.checked.lock().unwrap_or_else(|e| e.into_inner());
            c.retain(|_, (_, until)| Instant::now() < *until);
            c.insert(digest, (id, Instant::now() + TOKEN_CACHE));
        }
        Ok(Caller { token, node })
    }
}

async fn status(State(api): State<Arc<Api>>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    // The caller must be a researcher of one of the servers; the answer covers them all.
    api.caller_any(&headers).await?;
    let mut servers = Vec::new();
    for s in api.shared.served.read().await.iter() {
        let n = &s.node;
        let me: Value = n
            .call(|t| async move { n.api.get_json(&t, "/node/me").await })
            .await
            .unwrap_or(Value::Null);
        let held = n.held.lock().await;
        servers.push(json!({
            "name": s.entry.name, "url": s.entry.url, "network": s.entry.network,
            "node_id": n.id(),
            "projects": me["projects"],
            "held_artifacts": held.len(),
            "held_bytes": held.values().sum::<u64>(),
            "pulls_in_flight": n.in_flight.lock().unwrap_or_else(|e| e.into_inner()).len(),
            "pulls_failing": n.failing.lock().unwrap_or_else(|e| e.into_inner()).len(),
            "chunks_served": n.chunks_served(),
            "reachable_at": n.reachable_at().await.map(|(a, how)| json!({ "addr": a, "how": how })),
        }));
    }
    let waiting: Vec<Value> = api
        .shared
        .waiting
        .read()
        .await
        .iter()
        .map(|(e, why)| json!({ "url": e.url, "network": e.network, "why": why }))
        .collect();
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "servers": servers,
        "waiting": waiting,
    })))
}

impl Api {
    /// A caller of whichever server the token belongs to (for `/v1/status`).
    async fn caller_any(&self, headers: &HeaderMap) -> Result<(), ApiError> {
        if headers.get("x-gg-network").is_some() || self.shared.served.read().await.len() <= 1 {
            return self.caller(headers).await.map(|_| ());
        }
        let networks: Vec<String> = self
            .shared
            .served
            .read()
            .await
            .iter()
            .map(|s| s.entry.network.clone())
            .collect();
        for n in networks {
            let mut h = headers.clone();
            h.insert(
                "x-gg-network",
                n.parse()
                    .map_err(|_| err(StatusCode::BAD_REQUEST, "network"))?,
            );
            if self.caller(&h).await.is_ok() {
                return Ok(());
            }
        }
        Err(err(
            StatusCode::UNAUTHORIZED,
            "not a researcher token of any server",
        ))
    }
}

#[derive(Deserialize)]
struct DataQuery {
    #[serde(default)]
    schema: String,
}

/// A dataset: streamed to disk (bounded), kept in the node's store, declared for the project.
async fn put_data(
    State(api): State<Arc<Api>>,
    headers: HeaderMap,
    Path(project): Path<ProjectId>,
    Query(q): Query<DataQuery>,
    body: axum::body::Body,
) -> Result<Json<Value>, ApiError> {
    let c = api.caller(&headers).await?;
    // Membership is the control plane's answer; attachment, the node's own record there.
    let _: Value = c
        .node
        .api
        .get_json(&c.token, &format!("/researcher/projects/{project}"))
        .await?;
    let node = &c.node;
    let me: Value = node
        .call(|t| async move { node.api.get_json(&t, "/node/me").await })
        .await?;
    let attached = me["projects"]
        .as_array()
        .is_some_and(|p| p.iter().any(|v| v.as_str() == Some(&project.to_string())));
    if !attached {
        return Err(err(
            StatusCode::FORBIDDEN,
            "this node is not attached to the project: attach it first",
        ));
    }
    if q.schema.len() > 64 {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "schema label too long",
        ));
    }
    let dir = node.cache.root().join(".incoming");
    std::fs::create_dir_all(&dir).map_err(anyhow::Error::from)?;
    let tmp = dir.join(format!("upload-{}", uuid::Uuid::now_v7()));
    let written = async {
        use futures_util::StreamExt;
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::File::create(&tmp).await?;
        let mut stream = body.into_data_stream();
        let mut n: u64 = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| anyhow::anyhow!("upload: {e}"))?;
            n += chunk.len() as u64;
            if n > api.shared.cfg.max_data_bytes {
                anyhow::bail!("too large");
            }
            f.write_all(&chunk).await?;
        }
        f.sync_all().await?;
        anyhow::Ok(n)
    }
    .await;
    let r = match written {
        Ok(_) => node.hold(project, &tmp, &q.schema).await,
        Err(e) => Err(e),
    };
    let _ = std::fs::remove_file(&tmp);
    match r {
        Ok(r) => Ok(Json(json!({ "hash": r.hash, "size": r.size }))),
        Err(e) if e.to_string() == "too large" => Err(err(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("above max_data_bytes ({})", api.shared.cfg.max_data_bytes),
        )),
        Err(e) => Err(e.into()),
    }
}

/// A training run this node steps: the request is the control plane's, with `holder_node` set to
/// this node.
async fn create_run(
    State(api): State<Arc<Api>>,
    headers: HeaderMap,
    Path(project): Path<ProjectId>,
    Json(mut body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let c = api.caller(&headers).await?;
    let obj = body.as_object_mut().ok_or_else(|| {
        err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "a JSON object is expected",
        )
    })?;
    obj.insert("holder_node".into(), json!(c.node.id()));
    cut_tiles(&c, project, obj).await?;
    cut_stages(&c, project, obj).await?;
    let run: Option<Value> = c
        .node
        .api
        .post_json(
            &c.token,
            &format!("/researcher/projects/{project}/collective"),
            &body,
        )
        .await?;
    Ok(Json(run.unwrap_or(Value::Null)))
}

/// The description `graph` names, kept by the control plane, read as the researcher.
async fn description(
    c: &Caller,
    project: ProjectId,
    graph: &gg_protocol::messages::ArtifactRef,
) -> Result<Vec<u8>, ApiError> {
    if graph.size > gg_compute::graph::task::MAX_GRAPH_BYTES {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "graph artifact too large",
        ));
    }
    let dir = c.node.cache.root().join(".incoming");
    std::fs::create_dir_all(&dir).map_err(anyhow::Error::from)?;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dest = dir.join(format!("describe-{}-{seq}", graph.hash));
    c.node
        .api
        .download(
            &c.token,
            &format!("/researcher/projects/{project}/artifacts/{}", graph.hash),
            graph,
            graph.size,
            &dest,
        )
        .await?;
    let bytes = std::fs::read(&dest).map_err(anyhow::Error::from);
    let _ = std::fs::remove_file(&dest);
    Ok(bytes?)
}

fn graph_ref(
    obj: &serde_json::Map<String, Value>,
) -> Result<gg_protocol::messages::ArtifactRef, ApiError> {
    obj.get("graph")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .ok_or_else(|| {
            err(
                StatusCode::UNPROCESSABLE_ENTITY,
                "graph: an artifact reference",
            )
        })
}

/// The cutting by the node: every matrix above `tile_max_param_bytes` is split into tiles here.
/// The tiled description is kept by the control plane (it is small); the control plane is sent it
/// with the tiling and the source, and only checks that one gives the other.
async fn cut_tiles(
    c: &Caller,
    project: ProjectId,
    obj: &mut serde_json::Map<String, Value>,
) -> Result<(), ApiError> {
    let Some(max) = obj.get("tile_max_param_bytes").filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let max = max.as_u64().filter(|m| *m > 0).ok_or_else(|| {
        err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "tile_max_param_bytes must be a positive byte count",
        )
    })?;
    let source = graph_ref(obj)?;
    let bytes = description(c, project, &source).await?;
    let (tiled, tiling) = tile_description(&bytes, max)
        .map_err(|e| err(StatusCode::UNPROCESSABLE_ENTITY, format!("tiling: {e}")))?;
    obj.remove("tile_max_param_bytes");
    if tiling.is_empty() {
        return Ok(());
    }
    let tiled_ref = gg_protocol::messages::ArtifactRef {
        hash: Sha256Digest::of_bytes(&tiled),
        size: tiled.len() as u64,
    };
    c.node
        .api
        .put_bytes(
            &c.token,
            &format!(
                "/researcher/projects/{project}/artifacts/{}?size={}&schema=gg-graph-v1",
                tiled_ref.hash, tiled_ref.size
            ),
            tiled,
        )
        .await?;
    obj.insert("graph".into(), json!(tiled_ref));
    obj.insert("source_graph".into(), json!(source));
    obj.insert("tiling".into(), json!(tiling));
    Ok(())
}

/// The description `graph` (functions inlined, as the control plane reads it) with every matrix
/// above `max` bytes tiled, and the tiling. No tiling: the description is left as it is.
pub fn tile_description(
    graph: &[u8],
    max: u64,
) -> Result<(Vec<u8>, Vec<gg_compute::graph::tiling::Tiled>), String> {
    let submitted: gg_compute::graph::ModelGraphV1 =
        serde_json::from_slice(graph).map_err(|e| e.to_string())?;
    let model = gg_compute::graph::functions::inline(&submitted).map_err(|e| e.to_string())?;
    let (tiled, tiling) = gg_compute::graph::tiling::auto_tile(&model, max)?;
    if tiling.is_empty() {
        return Ok((graph.to_vec(), tiling));
    }
    Ok((
        serde_json::to_vec(&tiled).map_err(|e| e.to_string())?,
        tiling,
    ))
}

/// The cutting by the node: a model no machine holds whole is split into stages here, within
/// `stage_memory_bytes` per task, and the control plane is sent the bounds (it only checks them).
async fn cut_stages(
    c: &Caller,
    project: ProjectId,
    obj: &mut serde_json::Map<String, Value>,
) -> Result<(), ApiError> {
    let Some(cap) = obj.get("stage_memory_bytes").filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let cap = cap.as_u64().ok_or_else(|| {
        err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "stage_memory_bytes must be a byte count",
        )
    })?;
    if obj.get("stage_bounds").is_some_and(|v| !v.is_null()) {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "stage_bounds and stage_memory_bytes are exclusive",
        ));
    }
    let count = |k: &str| obj.get(k).and_then(Value::as_u64).filter(|n| *n > 0);
    let (Some(batch), Some(tasks)) = (count("batch_per_round"), count("tasks_per_round")) else {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "batch_per_round and tasks_per_round are needed to cut the model into stages",
        ));
    };
    let graph = graph_ref(obj)?;
    // The description is small and kept by the control plane (tiled already, if it was).
    let bytes = description(c, project, &graph).await;
    let bounds = stage_bounds(&bytes?, batch.div_ceil(tasks), cap)
        .map_err(|e| err(StatusCode::UNPROCESSABLE_ENTITY, format!("stages: {e}")))?;
    obj.remove("stage_memory_bytes");
    obj.insert("stage_bounds".into(), json!(bounds));
    Ok(())
}

/// Stage bounds of the description `graph` (as the control plane reads it: functions inlined, then
/// validated) such that no stage needs more than `cap` bytes for a micro-batch of `micro` examples.
pub fn stage_bounds(graph: &[u8], micro: u64, cap: u64) -> Result<Vec<u32>, String> {
    let submitted: gg_compute::graph::ModelGraphV1 =
        serde_json::from_slice(graph).map_err(|e| e.to_string())?;
    let model = gg_compute::graph::functions::inline(&submitted).map_err(|e| e.to_string())?;
    let report = gg_compute::graph::validate(&model).map_err(|e| e.to_string())?;
    gg_compute::graph::stages::partition(&model, &report, micro, cap)
}

/// The run as the control plane sees it, plus its current round's tasks by state and what the
/// node holds of its checkpoint.
async fn run_view(
    State(api): State<Arc<Api>>,
    headers: HeaderMap,
    Path(run): Path<uuid::Uuid>,
) -> Result<Json<Value>, ApiError> {
    let c = api.caller(&headers).await?;
    let view: Value = c
        .node
        .api
        .get_json(&c.token, &format!("/researcher/collective/{run}"))
        .await?;
    let mut tasks = BTreeMap::<String, u64>::new();
    let open = view["rounds"]
        .as_array()
        .and_then(|r| r.iter().rev().find(|r| r["state"] == "open"));
    if let Some(job) = open.and_then(|r| r["job_id"].as_str()) {
        let st: Value = c
            .node
            .api
            .get_json(&c.token, &format!("/researcher/jobs/{job}"))
            .await?;
        for t in st["tasks"].as_array().cloned().unwrap_or_default() {
            *tasks
                .entry(t["state"].as_str().unwrap_or("?").to_owned())
                .or_default() += 1;
        }
    }
    let held = c.node.held.lock().await;
    let checkpoint_here = view["checkpoint"]["weights"].as_object().is_some_and(|w| {
        !w.is_empty()
            && w.values().all(|r| {
                r["hash"]
                    .as_str()
                    .and_then(|h| h.parse::<Sha256Digest>().ok())
                    .is_some_and(|h| held.contains_key(&h))
            })
    });
    Ok(Json(json!({
        "run": view,
        "current_round_tasks": tasks,
        "checkpoint_on_this_node": checkpoint_here,
    })))
}

/// One part of the run's current checkpoint (`checkpoint`, or `checkpoint.i` when sharded), read
/// from this node's store: the trained model never passes through the control plane.
async fn checkpoint_part(
    State(api): State<Arc<Api>>,
    headers: HeaderMap,
    Path((run, name)): Path<(uuid::Uuid, String)>,
) -> Result<Response, ApiError> {
    let c = api.caller(&headers).await?;
    let view: Value = c
        .node
        .api
        .get_json(&c.token, &format!("/researcher/collective/{run}"))
        .await?;
    let r = &view["checkpoint"]["weights"][&name];
    let (Some(hash), Some(size)) = (
        r["hash"]
            .as_str()
            .and_then(|h| h.parse::<Sha256Digest>().ok()),
        r["size"].as_u64(),
    ) else {
        return Err(err(
            StatusCode::NOT_FOUND,
            "no such part in the current checkpoint",
        ));
    };
    let path = c.node.cache.path_of(&hash);
    let file = tokio::fs::File::open(&path).await.map_err(|_| {
        err(
            StatusCode::CONFLICT,
            "this node does not hold that checkpoint",
        )
    })?;
    let stream = tokio_util::io::ReaderStream::new(file);
    Ok((
        [
            ("content-type", "application/octet-stream".to_owned()),
            ("content-length", size.to_string()),
            ("x-gg-sha256", hash.to_hex()),
        ],
        axum::body::Body::from_stream(stream),
    )
        .into_response())
}

async fn cancel_run(
    State(api): State<Arc<Api>>,
    headers: HeaderMap,
    Path(run): Path<uuid::Uuid>,
) -> Result<StatusCode, ApiError> {
    let c = api.caller(&headers).await?;
    let _: Option<Value> = c
        .node
        .api
        .post_json(
            &c.token,
            &format!("/researcher/collective/{run}/cancel"),
            &json!({}),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The API's routes.
fn router(shared: Arc<Shared>) -> axum::Router {
    let api = Arc::new(Api {
        shared,
        checked: std::sync::Mutex::new(BTreeMap::new()),
    });
    axum::Router::new()
        .route("/v1/status", get(status))
        .route("/v1/projects/{project}/data", put(put_data))
        .route("/v1/projects/{project}/runs", post(create_run))
        .route("/v1/runs/{run}", get(run_view))
        .route("/v1/runs/{run}/checkpoint/{name}", get(checkpoint_part))
        .route("/v1/runs/{run}/cancel", post(cancel_run))
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(api)
}

/// The TLS configuration of an API that listens on the network.
fn tls_config(
    cert: &std::path::Path,
    key: &std::path::Path,
) -> anyhow::Result<Arc<rustls::ServerConfig>> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .with_context_path(cert)?
        .collect::<Result<_, _>>()
        .with_context_path(cert)?;
    let key = PrivateKeyDer::from_pem_file(key).with_context_path(key)?;
    let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(certs, key)?;
    Ok(Arc::new(cfg))
}

trait WithPath<T> {
    fn with_context_path(self, p: &std::path::Path) -> anyhow::Result<T>;
}

impl<T, E: std::fmt::Display> WithPath<T> for Result<T, E> {
    fn with_context_path(self, p: &std::path::Path) -> anyhow::Result<T> {
        self.map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))
    }
}

/// Serve the local API until `stop` turns true.
pub async fn serve(
    shared: Arc<Shared>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let cfg = shared.cfg.clone();
    let listener = tokio::net::TcpListener::bind(&cfg.api_listen)
        .await
        .map_err(|e| anyhow::anyhow!("local API on {}: {e}", cfg.api_listen))?;
    tracing::info!(addr = %listener.local_addr()?, tls = cfg.api_tls_cert.is_some(), "local API listening");
    let app = router(shared);
    match (&cfg.api_tls_cert, &cfg.api_tls_key) {
        (Some(cert), Some(key)) => {
            let tls = tls_config(cert, key)?;
            gg_client::peer_tls::serve_tls(listener, app, tls, 64, stop).await;
        }
        _ => {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = stop.wait_for(|v| *v).await;
                })
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use gg_compute::graph::templates::{Positions, TransformerDims, transformer};

    #[test]
    fn the_node_cuts_a_model_within_the_memory_given() {
        let model = transformer(TransformerDims {
            layers: 2,
            dim: 16,
            heads: 2,
            seq: 4,
            vocab: 12,
            ffn: 32,
            causal: true,
            positions: Positions::rotary(10000.0),
        })
        .unwrap();
        let report = gg_compute::graph::validate(&model).unwrap();
        let whole =
            gg_compute::graph::memory::task_memory(&model, &report, 4, Default::default()).total();
        let bytes = serde_json::to_vec(&model).unwrap();
        let bounds = super::stage_bounds(&bytes, 4, whole / 3).unwrap();
        assert!(bounds.len() > 3, "{bounds:?}");
        let stages = gg_compute::graph::stages::stages(&model, &report, &bounds).unwrap();
        for s in &stages {
            assert!(gg_compute::graph::stages::stage_memory(&model, &report, s, 4) <= whole / 3);
        }
        // A cap nothing fits in is refused, not cut anyway.
        assert!(super::stage_bounds(&bytes, 4, 1).is_err());
        assert!(super::stage_bounds(b"not a graph", 4, whole).is_err());
    }
}
