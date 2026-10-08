//! Chunks relayed by the control plane for a lab node nobody can reach directly (ADR-0041, A8).
//! Decided with the owner: the server relays small artifacts only (`node_relay_max_bytes`).
//!
//! A node behind a NAT it cannot open still reaches the control plane. It waits on
//! `GET /node/relay` for chunk requests; when a worker or a validator asks the control plane for an
//! artifact the node holds, the server queues one request per chunk for that node, the node sends
//! each chunk (`PUT /node/relay/{id}`), and the server hands it to the waiting reader. Nothing is
//! stored: the bytes live in memory for as long as one chunk takes to pass through. The reader
//! checks every chunk against the manifest the server signed and the whole against its hash, as for
//! any peer, so a relay cannot alter what it carries.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use axum::body::Body;
use axum::response::Response;
use gg_core::{NodeId, Sha256Digest};
use serde::Serialize;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// Chunk requests queued for one node at most: beyond, readers are told to retry.
const MAX_QUEUED: usize = 256;

/// A chunk the node is asked to send.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct RelayItem {
    pub request_id: Uuid,
    pub hash: Sha256Digest,
    pub index: u32,
    /// Byte range of the chunk in the artifact.
    pub start: u64,
    pub len: u64,
}

struct Waiting {
    node: NodeId,
    len: u64,
    reply: tokio::sync::oneshot::Sender<Vec<u8>>,
}

#[derive(Default)]
struct Inner {
    queues: BTreeMap<NodeId, VecDeque<RelayItem>>,
    waiting: BTreeMap<Uuid, Waiting>,
}

/// The rendezvous between readers and nodes.
#[derive(Default)]
pub struct RelayHub {
    inner: Mutex<Inner>,
    /// Woken when a request is queued for any node.
    pub queued: tokio::sync::Notify,
}

impl RelayHub {
    /// Ask `node` for one chunk and wait for it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn chunk(
        &self,
        wait: Duration,
        node: NodeId,
        hash: Sha256Digest,
        index: u32,
        start: u64,
        len: u64,
    ) -> Result<Vec<u8>, ApiError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let id = Uuid::now_v7();
        {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let q = g.queues.entry(node).or_default();
            if q.len() >= MAX_QUEUED {
                return Err(ApiError::Unavailable(
                    "the lab node's relay queue is full: retry".into(),
                ));
            }
            q.push_back(RelayItem {
                request_id: id,
                hash,
                index,
                start,
                len,
            });
            g.waiting.insert(
                id,
                Waiting {
                    node,
                    len,
                    reply: tx,
                },
            );
        }
        self.queued.notify_waiters();
        let got = tokio::time::timeout(wait, rx).await;
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.waiting.remove(&id);
        if let Some(q) = g.queues.get_mut(&node) {
            q.retain(|i| i.request_id != id);
        }
        match got {
            Ok(Ok(bytes)) => Ok(bytes),
            _ => Err(ApiError::Unavailable(
                "the lab node holding this artifact did not send it in time (offline?)".into(),
            )),
        }
    }

    /// The requests waiting for `node`, taken off its queue.
    pub fn take(&self, node: NodeId, max: usize) -> Vec<RelayItem> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(q) = g.queues.get_mut(&node) else {
            return Vec::new();
        };
        let n = q.len().min(max);
        q.drain(..n).collect()
    }

    /// A chunk from `node` for request `id`: handed to its reader if it waits and the size is the
    /// one asked for. Returns whether it was taken.
    pub fn deliver(&self, node: NodeId, id: Uuid, bytes: Vec<u8>) -> bool {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match g.waiting.get(&id) {
            Some(w) if w.node == node && w.len == bytes.len() as u64 => {}
            _ => return false,
        }
        let w = g.waiting.remove(&id).expect("checked just above");
        w.reply.send(bytes).is_ok()
    }
}

/// Whether the control plane relays this artifact: held by a lab node that readers cannot reach
/// (it has no live announcement for it), not stored here, and small enough
/// (`node_relay_max_bytes`). A reachable node is read directly, through peers.
pub async fn relayable(
    state: &AppState,
    hash: &Sha256Digest,
) -> Result<Option<(NodeId, u64)>, ApiError> {
    if !state.config().p2p_enabled || state.objects().exists(hash) {
        return Ok(None);
    }
    let mut c = state.db().pool().acquire().await?;
    let Some(node) = gg_storage::queries::artifacts::held_by_node(&mut c, hash).await? else {
        return Ok(None);
    };
    let Some(row) = gg_storage::queries::artifacts::get_artifact(&mut c, hash).await? else {
        return Ok(None);
    };
    let size = row.size as u64;
    if size > state.config().node_relay_max_bytes
        || gg_storage::queries::p2p::node_announced(&mut c, hash, node).await?
    {
        return Ok(None);
    }
    Ok(Some((node, size)))
}

/// Serve `range` (or the whole artifact) of an artifact `node` holds, chunk by chunk through the
/// node, as a stream: at most one chunk is in memory at a time.
pub async fn stream(
    state: &AppState,
    node: NodeId,
    hash: Sha256Digest,
    size: u64,
    range_header: Option<&str>,
) -> Result<Response, ApiError> {
    let row = crate::workflows::p2p::chunk_digests(state, &hash).await?;
    let chunk = row.chunk_size as u64;
    let (status, start, len) = match crate::api::researcher::parse_range(range_header, size) {
        Ok(Some((s, e))) => (206, s, e - s + 1),
        Ok(None) => (200, 0, size),
        Err(_) => {
            return Response::builder()
                .status(416)
                .header("content-range", format!("bytes */{size}"))
                .body(Body::empty())
                .map_err(|e| ApiError::Internal(e.to_string()));
        }
    };
    let end = start + len; // exclusive
    let wait = Duration::from_secs(state.config().node_relay_wait_secs.max(1));
    let first = if len == 0 { 0 } else { start / chunk };
    let last = if len == 0 { 0 } else { (end - 1) / chunk + 1 };
    // The first chunk before answering: a node that does not answer gives a clean 503 (retried by
    // the clients), not a 200 whose body breaks off.
    let slice = move |bytes: Vec<u8>, c_start: u64, c_len: u64| {
        let from = start.saturating_sub(c_start) as usize;
        let to = ((end - c_start).min(c_len)) as usize;
        axum::body::Bytes::from(bytes).slice(from..to)
    };
    let span = move |i: u64| (i * chunk, chunk.min(size - i * chunk));
    let head = if first < last {
        let (c_start, c_len) = span(first);
        let bytes = state
            .relay()
            .chunk(wait, node, hash, first as u32, c_start, c_len)
            .await?;
        Some(slice(bytes, c_start, c_len))
    } else {
        None
    };
    let hub_state = state.clone();
    let rest = futures_util::stream::unfold(first + 1, move |i| {
        let state = hub_state.clone();
        async move {
            if i >= last {
                return None;
            }
            let (c_start, c_len) = span(i);
            let part = match state
                .relay()
                .chunk(wait, node, hash, i as u32, c_start, c_len)
                .await
            {
                Ok(bytes) => Ok(slice(bytes, c_start, c_len)),
                Err(e) => Err(std::io::Error::other(e.to_string())),
            };
            Some((part, i + 1))
        }
    });
    use futures_util::StreamExt;
    let stream = futures_util::stream::iter(head.map(Ok::<_, std::io::Error>)).chain(rest);
    let mut b = Response::builder()
        .status(status)
        .header("content-type", "application/octet-stream")
        .header("content-length", len)
        .header("accept-ranges", "bytes")
        .header("x-gg-relayed", "lab-node");
    if status == 206 {
        b = b.header("content-range", format!("bytes {start}-{}/{size}", end - 1));
    }
    b.body(Body::from_stream(stream))
        .map_err(|e| ApiError::Internal(e.to_string()))
}
