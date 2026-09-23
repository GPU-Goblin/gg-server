# ADR-0012 — Peer-to-peer artifact transport

Status: accepted and implemented (2026-09-17), phase P5.

## Context

Artifacts (datasets, the checkpoints of a collective run) are immutable and addressed by SHA-256. In P4,
a 4-task round makes every worker download the same 6 inputs from the control plane: each byte leaves
the server as many times as there are workers. Once the base protocol is in place, the original
requirements call for: an `ArtifactTransport` interface, **a proven transport rather than a new network
stack**, an evaluation of an existing Rust BitTorrent adapter, a link between the signed application
manifest and the transport's chunks, an HTTP fallback source, connection limits, a malicious-peer test,
documentation of address visibility, and outbound sharing that is explicit, bounded and can be turned off.

## Evaluation of BitTorrent (measured on 2026-09-17)

| Item | Finding |
|---|---|
| `librqbit` 9.0.1 (rqbit client) | No occurrence of "BitTorrent v2", BEP 52 or "merkle" in the README or in `src/` of the published release: **v1 only**, so pieces are hashed with **SHA-1**. |
| `cratetorrent` 0.1.0 | Described as "a simple BitTorrent V1 engine library", version 0.1.0, unmaintained. |
| Surface | `librqbit` bundles DHT, tracker communication and UPnP (`librqbit-dht`, `librqbit-tracker-comms`, `librqbit-upnp`), ~167 lines of dependencies. A private swarm is possible (`AddTorrentOptions::initial_peers`, with the explicit error message "no known way to resolve peers (no DHT, no trackers, no initial_peers)"), but those subsystems remain in the build tree. |
| Consequence | Our artifacts are identified by SHA-256; BitTorrent v1 would impose a second identity system (infohash + SHA-1 pieces) to maintain and bind, built on an integrity primitive we refuse for verification. BitTorrent v2 (SHA-256, Merkle trees) is not available today in a Rust library we can verify. |

Claiming BitTorrent v2 compatibility would be false; building a v1 swarm would add a competing network
and a second hashing scheme for no gain in our use case (few peers, all authenticated by the control
plane, objects of a few megabytes).

## Decision

A minimal peer-to-peer transport that **reuses the network stack already in place** (axum on the chunk
server side, reqwest on the reader side, the same libraries as the rest of the product), not a new stack:

- **Signed chunk manifest**: `ChunkManifestV1 { artifact_hash, size, chunk_size, chunks[] }`
  (domain `chunk-manifest`). The control plane computes the SHA-256 of each chunk **from its own
  bytes**, stores them (`artifact_chunks`) and signs the manifest. This is the required link between the
  signed application manifest and the transport's chunks: a reader verifies each chunk before writing it,
  then re-verifies the hash of the whole artifact.
- **The control plane is the tracker**: `POST /v1/worker/artifacts/{hash}/announce` (the worker declares
  that it holds the artifact at `ip:port`, bounded TTL), `GET …/peers` (bounded list + ticket), `GET …/manifest`.
  All three routes require the worker to be *already* authorized to read the artifact (an assigned task),
  the same rule as for HTTP downloads. Since ADR-0026, a worker that **itself uploaded** the artifact as a
  task output may also announce it: this is the direct hand-off of an activation to the next stage.
- **One connection per peer per transfer** (2026-09-23): the reader opens a TLS session on the first
  chunk and reuses it for the following ones; a session that fails is dropped and the next one is
  reopened. On the holder side, a connection idle between two requests is closed after 30 s. Measured
  behind a relay that adds 20 ms in each direction (`tests/p2p.rs`): 8 chunks in 688 ms with one
  connection per chunk, 389 ms with a single connection, i.e. one round trip saved per chunk (TLS 1.3
  already sends the request along with the end of the handshake).
- **Several chunks in flight** (2026-09-23, `p2p.parallel_chunks`, 4 by default): each chunk is
  verified against the manifest as it arrives, the file is written in order and hashed in a single
  pass, and at most `parallel_chunks` chunks wait in memory. Same relay, 16 chunks: 734 ms one at a
  time, **221 ms** four at a time. A lying peer is still dropped, and nothing it sends is written. Only
  the requests already sent to it before detection can reach it, at most `parallel_chunks`; with one
  chunk at a time it is queried only once (both cases are tested).
- **Swarm** (2026-09-23): workers starting together on the same input all fetched it from the control
  plane (measured: 0 of 30 chunks exchanged in `demo --p2p`). Now:
  - a worker that is downloading announces itself as a holder **from the start**. Its chunk server serves
    every chunk already verified and written (`Partials`), and answers 404 for the others;
  - each reader walks through the chunks in its own order, derived from its key;
  - each chunk has an owner among the listed readers, chosen by rendezvous hashing on each reader's
    published TLS fingerprint. The owner fetches the chunk from a peer that has it, otherwise from the
    control plane. The others wait for it at the owner's, for at most `swarm_patience_ms` (2 s), then
    request it from the control plane: a slow or vanished owner blocks nothing;
  - a finished download keeps being served from its file until it enters the cache.

  Verification is unchanged: each chunk is verified against the signed manifest, then the whole artifact
  against its hash. Measurements:
  - two simultaneous readers, 16 chunks, control plane behind a 40 ms round trip
    (`tests/p2p.rs`): **16 chunks requested from the control plane instead of 32**, i.e. a single
    copy, in ~250 ms;
  - `demo --p2p` with workers started together: 18 to 20 chunks exchanged between them, 10 to 12
    from the control plane, out of 30 requested.

  Two earlier attempts, measured and abandoned before reaching this design:
  - "set aside what no peer has": at startup nobody has anything, so everything was set aside and then
    fetched from the control plane;
  - "staggered starting order": two neighboring starting points requested the same chunks at the same
    time.
- **Signed read ticket**: `PeerTicketV1 { artifact_hash, size, chunk_size, reader_id, reader_key,
  expires_at }` (domain `peer-ticket`). The holder verifies it offline with the server's key; the
  reader proves possession of `reader_key` by signing each `ChunkRequestV1` (domain
  `chunk-request`, 120 s freshness). **A peer never makes an authorization decision** and never sees the
  reader's session token. `size` and `chunk_size` are in the ticket: the offsets served are computed from
  data signed by the server, not from a parameter supplied by the requester.
- **Per-chunk HTTP fallback**: any chunk that no peer serves correctly is requested from the control
  plane (`Range`), which remains the **durable source**; any failure of the peer-to-peer path falls back
  to P3's resumable HTTP download.
- **Malicious peer**: a chunk whose SHA-256 does not match the manifest is discarded and the peer is
  excluded for the rest of the transfer. No unverified byte reaches the file.
- **Explicit, bounded outbound sharing that can be turned off**: `[p2p] enabled = false` by default,
  `share`, `max_upload_conns`, `upload_budget_bytes`, `listen_addr`/`advertise_addr`. Only artifacts
  present in the cache (and therefore downloaded for a task) are served; the served path is derived from
  the hash, never from a name supplied by the caller. On the server side, `p2p_enabled = false` disables
  the tracker.
- **Address visibility**: an announcement makes `ip:port` visible to the workers authorized for that
  artifact (not to researchers, not to the public). Non-routable addresses (0.0.0.0, multicast,
  link-local, port 0) are always refused; loopback and private ranges only if `p2p_allow_private_addrs`
  is set on the server and `p2p.allow_private_peers` on the worker — both refused in public mode.
- **Cache**: bounded, LRU, with **pinning** of the artifacts of an attempt in progress (never evicted);
  every cache read is re-hashed.

## Consequences

- Peer-to-peer distributes immutable bytes. It does not distribute leases, validation authority or
  decisions: the control plane alone signs manifests and tickets, and holds the only durable copy.
- An offline, lying or slow worker can at most make a transfer fall back to HTTP.
- This transport does not traverse NATs and has no discovery outside the tracker: it targets a lab or a
  set of reachable machines, not a public swarm. An extension (relays, uTP, BitTorrent v2 once a
  verifiable Rust library exists) will get its own ADR; we will not maintain two competing networks side
  by side.
- The corresponding figures and tests are in docs/STATUS.md and `crates/gg-worker/tests/p2p.rs`.
