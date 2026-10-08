//! The reversed connection (ADR-0041, A8): a reader nobody's holder can be reached from, but that
//! can itself be reached, asks the control plane to have the holders connect to it and push the
//! chunks. The control plane only puts the two sides in touch. It dials the reader's address back,
//! signs a push ticket naming the reader's transport key, and hands the request to each holder (a
//! lab node, or the worker that produced an output), which is waiting for it (`GET …/pushes`). No
//! byte of the artifact passes through the server.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use gg_core::*;
use gg_protocol::domain::Domain;
use gg_protocol::messages::PushTicketV1;
use gg_protocol::signed::Signed;
use serde::Serialize;

use crate::error::ApiError;
use crate::state::AppState;

/// Who holds what is asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Holder {
    Node(NodeId),
    Worker(WorkerId),
}

/// A push a holder is asked for.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct PushRequest {
    pub hash: Sha256Digest,
    pub size: u64,
    pub chunk_size: u32,
    /// Where the reader listens (dialled back).
    pub reader_addr: String,
    /// The reader's transport key: the holder pins it.
    pub reader_fingerprint: Sha256Digest,
    /// Hex of the server-signed push ticket.
    pub ticket: String,
}

/// Requests queued for one holder at most.
const MAX_QUEUED: usize = 256;

/// The requests waiting for each holder.
#[derive(Default)]
pub struct PushHub {
    queues: Mutex<BTreeMap<Holder, VecDeque<PushRequest>>>,
    /// Woken when a request is queued.
    pub queued: tokio::sync::Notify,
}

impl PushHub {
    fn push(&self, holder: Holder, req: PushRequest) -> bool {
        let mut g = self.queues.lock().unwrap_or_else(|e| e.into_inner());
        let q = g.entry(holder).or_default();
        if q.len() >= MAX_QUEUED {
            return false;
        }
        // The same reader asking twice for the same artifact: once is enough.
        if q.iter()
            .any(|r| r.hash == req.hash && r.reader_fingerprint == req.reader_fingerprint)
        {
            return true;
        }
        q.push_back(req);
        drop(g);
        self.queued.notify_waiters();
        true
    }

    /// The requests waiting for `holder`, taken off its queue.
    pub fn take(&self, holder: Holder, max: usize) -> Vec<PushRequest> {
        let mut g = self.queues.lock().unwrap_or_else(|e| e.into_inner());
        let Some(q) = g.get_mut(&holder) else {
            return Vec::new();
        };
        let n = q.len().min(max);
        q.drain(..n).collect()
    }

    /// Wait up to `wait_secs` for requests for `holder`.
    pub async fn wait(&self, holder: Holder, wait_secs: u64) -> Vec<PushRequest> {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(wait_secs.min(30));
        loop {
            let notified = self.queued.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let items = self.take(holder, 64);
            if !items.is_empty() {
                return items;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Vec::new();
            }
            let _ = tokio::time::timeout(left, notified).await;
        }
    }
}

/// The reader (already authorized for `hash` by the caller) asks to be pushed `hash` at `addr`,
/// reachable over TLS with `fingerprint`. Returns how many holders were asked.
pub async fn ask(
    state: &AppState,
    hash: &Sha256Digest,
    addr: &str,
    fingerprint: &Sha256Digest,
) -> Result<usize, ApiError> {
    if !state.config().p2p_enabled {
        return Err(ApiError::NotFound("peer distribution is disabled".into()));
    }
    // The reader must be reachable where it says: the holders are sent there.
    let parsed = super::p2p::parse_addr(state, addr)?;
    super::p2p::dial_back(state, &parsed).await?;
    let row = super::p2p::chunk_digests(state, hash).await?;
    let now = UnixMillis::now();
    let ticket = PushTicketV1 {
        protocol_version: gg_protocol::PROTOCOL_VERSION,
        network: state.network().clone(),
        artifact_hash: *hash,
        size: row.size,
        chunk_size: row.chunk_size,
        reader_fingerprint: *fingerprint,
        issued_at: now,
        expires_at: UnixMillis(now.0 + state.config().p2p_ticket_ttl_secs * 1000),
    };
    let signed = Signed::sign(
        state.server_key(),
        state.network(),
        Domain::PushTicket,
        ticket,
    )
    .map_err(|e| ApiError::Internal(e.to_string()))?;
    let ticket_hex = hex::encode(
        signed
            .to_canonical_bytes()
            .map_err(|e| ApiError::Internal(e.to_string()))?,
    );
    let req = PushRequest {
        hash: *hash,
        size: row.size,
        chunk_size: row.chunk_size,
        reader_addr: parsed.to_string(),
        reader_fingerprint: *fingerprint,
        ticket: ticket_hex,
    };
    // The holders that may not be reachable: the lab node that holds it, the worker that produced
    // it and still keeps it. (Holders that announced an address are read directly.)
    let mut holders = Vec::new();
    {
        let mut c = state.db().pool().acquire().await?;
        if let Some(node) = gg_storage::queries::artifacts::held_by_node(&mut c, hash).await? {
            holders.push(Holder::Node(node));
        }
        if let Some(p) = gg_storage::queries::pending::get(&mut c, hash).await? {
            holders.push(Holder::Worker(WorkerId(p.worker_id)));
        }
    }
    let mut asked = 0;
    for h in holders {
        if state.push().push(h, req.clone()) {
            asked += 1;
        }
    }
    if asked == 0 {
        return Err(ApiError::NotFound(
            "no holder of this artifact to ask for a push".into(),
        ));
    }
    Ok(asked)
}
