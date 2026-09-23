//! Peer-assisted artifact distribution (ADR-0012), control-plane side.
//!
//! The control plane keeps the durable copy and remains the only authority: it computes the chunk
//! digests over its own bytes and signs them (except for an activation still travelling before its
//! durable copy, whose digests its producer declared, ADR-0026), it decides who may read an artifact (short-lived
//! signed tickets), and it keeps a bounded, expiring registry of who announced holding what. Peers
//! move bytes; they never grant access, never extend a lease and never validate a result.

use std::net::{IpAddr, SocketAddr};

use gg_core::*;
use gg_protocol::domain::Domain;
use gg_protocol::messages::{ChunkManifestV1, PeerTicketV1};
use gg_protocol::signed::Signed;
use gg_storage::queries::p2p;
use serde::Serialize;
use tokio::io::AsyncReadExt;

use crate::error::ApiError;
use crate::state::AppState;

/// A peer as advertised to an authorized reader.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct PeerEntry {
    /// Worker record of the holder.
    pub worker_id: WorkerId,
    /// `host:port` the holder announced.
    pub addr: String,
    /// SHA-256 of the holder's TLS transport key (RFC 7250 raw public key). A reader pins it: the
    /// handshake succeeds only against the key the control plane recorded (ADR-0013).
    pub tls_fingerprint: Sha256Digest,
}

/// Peers plus the ticket the reader must present to them.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct PeersResponse {
    /// Live holders (bounded, excluding the caller).
    pub peers: Vec<PeerEntry>,
    /// Ticket signed by the server, valid for this artifact and this reader only.
    #[schema(value_type = Object)]
    pub ticket: Signed<PeerTicketV1>,
}

/// Whether an announced address may be handed to other workers. Unspecified, multicast,
/// broadcast, link-local and documentation addresses are always refused; loopback and private
/// ranges only when the deployment allows them (single-host demos, laboratory networks).
pub fn addr_allowed(addr: &SocketAddr, allow_private: bool) -> bool {
    if addr.port() == 0 {
        return false;
    }
    match addr.ip() {
        IpAddr::V4(v4) => {
            if v4.is_unspecified() || v4.is_multicast() || v4.is_broadcast() || v4.is_link_local() {
                return false;
            }
            if v4.is_loopback() || v4.is_private() {
                return allow_private;
            }
            true
        }
        IpAddr::V6(v6) => {
            if v6.is_unspecified() || v6.is_multicast() {
                return false;
            }
            let seg = v6.segments()[0];
            let link_local = seg & 0xffc0 == 0xfe80;
            let unique_local = seg & 0xfe00 == 0xfc00;
            if v6.is_loopback() || link_local || unique_local {
                return allow_private;
            }
            true
        }
    }
}

/// Parse and check an announced address against the deployment policy.
pub fn parse_addr(state: &AppState, addr: &str) -> Result<SocketAddr, ApiError> {
    if addr.len() > 64 {
        return Err(ApiError::Unprocessable("address too long".into()));
    }
    let parsed: SocketAddr = addr
        .parse()
        .map_err(|_| ApiError::Unprocessable("address must be ip:port".into()))?;
    let allow_private = state.config().p2p_allow_private_addrs || state.config().dev_mode;
    if !addr_allowed(&parsed, allow_private) {
        return Err(ApiError::Unprocessable(
            "address not allowed by the peer address policy".into(),
        ));
    }
    Ok(parsed)
}

/// Chunk digests of an artifact, computed once from the stored object and reused afterwards.
/// The digests are always computed by the server over its own bytes, never taken from a client.
pub async fn chunk_digests(
    state: &AppState,
    hash: &Sha256Digest,
) -> Result<p2p::ChunkRow, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if let Some(row) = p2p::get_chunks(&mut c, hash).await? {
        return Ok(row);
    }
    if !state.objects().exists(hash)
        && let Some(p) = gg_storage::queries::pending::get(&mut c, hash).await?
    {
        // Activation passed on before its durable copy (ADR-0026): the manifest carries the
        // digests declared by the producer. It certifies the integrity of the transfer, not the
        // computation; the reader re-checks the full hash, the one in the signed submission.
        // Nothing is kept: the server's digests will be computed over its own bytes on arrival.
        return Ok(p2p::ChunkRow {
            size: p.size,
            chunk_size: p.chunk_size,
            digests: p.digests,
        });
    }
    let chunk_size = state.config().p2p_chunk_bytes;
    let size = state.objects().size(hash).await?;
    let mut file = state.objects().open_read(hash).await?;
    let mut digests = Vec::with_capacity(size.div_ceil(chunk_size as u64) as usize);
    let mut buf = vec![0u8; chunk_size as usize];
    let mut total = 0u64;
    loop {
        let mut filled = 0usize;
        while filled < buf.len() {
            let n = file
                .read(&mut buf[filled..])
                .await
                .map_err(|e| ApiError::Internal(format!("read object: {e}")))?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            break;
        }
        total += filled as u64;
        digests.push(Sha256Digest::of_bytes(&buf[..filled]));
        if filled < buf.len() {
            break;
        }
    }
    if total != size {
        return Err(ApiError::Internal(format!(
            "object {hash} changed while chunking ({total} read, {size} expected)"
        )));
    }
    p2p::insert_chunks(&mut c, hash, size, chunk_size, &digests).await?;
    Ok(p2p::ChunkRow {
        size,
        chunk_size,
        digests,
    })
}

/// Signed chunk manifest of an artifact.
pub async fn manifest(
    state: &AppState,
    hash: &Sha256Digest,
) -> Result<Signed<ChunkManifestV1>, ApiError> {
    let row = chunk_digests(state, hash).await?;
    let body = ChunkManifestV1 {
        protocol_version: gg_protocol::PROTOCOL_VERSION,
        network: state.network().clone(),
        artifact_hash: *hash,
        size: row.size,
        chunk_size: row.chunk_size,
        chunks: row.digests,
        issued_at: UnixMillis::now(),
    };
    body.validate_structure()
        .map_err(|e| ApiError::Internal(format!("manifest: {e}")))?;
    Signed::sign(
        state.server_key(),
        state.network(),
        Domain::ChunkManifest,
        body,
    )
    .map_err(|e| ApiError::Internal(e.to_string()))
}

/// Record that `worker` holds `hash` at `addr` (the caller has already checked authorization).
pub async fn announce(
    state: &AppState,
    worker: WorkerId,
    hash: &Sha256Digest,
    addr: &str,
    tls_fingerprint: &Sha256Digest,
) -> Result<(), ApiError> {
    let addr = parse_addr(state, addr)?;
    dial_back(state, &addr).await?;
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    p2p::announce_peer(
        &mut c,
        hash,
        worker,
        &addr.to_string(),
        tls_fingerprint,
        state.config().p2p_peer_ttl_secs,
    )
    .await?;
    Ok(())
}

/// Check that the announced address actually accepts a connection before other workers are sent
/// there. Nothing is written to the socket and nothing is read: a worker behind a NAT without a
/// mapping is simply not advertised, and the registry keeps no unreachable entries (ADR-0013).
pub async fn dial_back(state: &AppState, addr: &SocketAddr) -> Result<(), ApiError> {
    if !state.config().p2p_dial_back {
        return Ok(());
    }
    let timeout = std::time::Duration::from_millis(state.config().p2p_dial_back_timeout_ms);
    match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr)).await {
        Ok(Ok(stream)) => {
            drop(stream);
            Ok(())
        }
        Ok(Err(e)) => Err(ApiError::Unprocessable(format!(
            "announced address {addr} is not reachable from the control plane: {e}"
        ))),
        Err(_) => Err(ApiError::Unprocessable(format!(
            "announced address {addr} did not answer within {} ms",
            timeout.as_millis()
        ))),
    }
}

/// Live holders of `hash` plus a ticket for `worker` (the caller has checked authorization).
pub async fn peers(
    state: &AppState,
    worker: WorkerId,
    worker_key: &gg_protocol::keys::PublicKey,
    hash: &Sha256Digest,
) -> Result<PeersResponse, ApiError> {
    let row = chunk_digests(state, hash).await?;
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let rows = p2p::list_peers(&mut c, hash, worker, state.config().p2p_max_peers).await?;
    let now = UnixMillis::now();
    let ticket = PeerTicketV1 {
        protocol_version: gg_protocol::PROTOCOL_VERSION,
        network: state.network().clone(),
        artifact_hash: *hash,
        size: row.size,
        chunk_size: row.chunk_size,
        reader_id: worker,
        reader_key: *worker_key,
        issued_at: now,
        expires_at: UnixMillis(now.0 + state.config().p2p_ticket_ttl_secs * 1000),
    };
    Ok(PeersResponse {
        peers: rows
            .into_iter()
            .map(|(worker_id, addr, tls_fingerprint)| PeerEntry {
                worker_id,
                addr,
                tls_fingerprint,
            })
            .collect(),
        ticket: Signed::sign(
            state.server_key(),
            state.network(),
            Domain::PeerTicket,
            ticket,
        )
        .map_err(|e| ApiError::Internal(e.to_string()))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_policy_refuses_unroutable_and_gates_private_ranges() {
        let public: SocketAddr = "203.0.113.7:9000".parse().unwrap();
        let loopback: SocketAddr = "127.0.0.1:9000".parse().unwrap();
        let private: SocketAddr = "192.168.1.10:9000".parse().unwrap();
        let link_local: SocketAddr = "169.254.1.1:9000".parse().unwrap();
        let any: SocketAddr = "0.0.0.0:9000".parse().unwrap();
        let zero_port: SocketAddr = "203.0.113.7:0".parse().unwrap();
        let v6_local: SocketAddr = "[::1]:9000".parse().unwrap();
        let v6_public: SocketAddr = "[2001:db8::1]:9000".parse().unwrap();
        for allow in [false, true] {
            assert!(addr_allowed(&public, allow));
            assert!(addr_allowed(&v6_public, allow));
            assert!(!addr_allowed(&link_local, allow));
            assert!(!addr_allowed(&any, allow));
            assert!(!addr_allowed(&zero_port, allow));
        }
        assert!(!addr_allowed(&loopback, false));
        assert!(!addr_allowed(&private, false));
        assert!(!addr_allowed(&v6_local, false));
        assert!(addr_allowed(&loopback, true));
        assert!(addr_allowed(&private, true));
        assert!(addr_allowed(&v6_local, true));
    }
}
