# ADR-0013 — Peer encryption and NAT traversal

Status: accepted and implemented (2026-09-17), complements ADR-0012 (phase P5).

## Context

ADR-0012 delivered peer-to-peer distribution: chunks verified against a manifest signed by the server,
HTTP fallback, bounded sharing. Two gaps were identified:

1. **Confidentiality and peer authentication**: chunks traveled in the clear. A network observer could
   see the bytes of a dataset or checkpoint and which artifacts were requested; nothing stopped a third
   party in an interception position from impersonating the announced holder (at worst it would serve
   wrong bytes, immediately rejected by the manifest — but it would still learn what was requested and
   could exhaust the retries).
2. **Reachability**: a contributor behind a home NAT has no address it can announce, and therefore shares
   nothing.

## Decision — encryption

TLS 1.3 is **mandatory** between peers, with **RFC 7250 raw public keys** (no X.509, no certificate
authority, no hostname, no plaintext fallback):

- A worker that enables sharing generates an **ephemeral transport key** (Ed25519) at startup.
  It is **never** its protocol identity key: the identity key signs only protocol messages, the transport
  key signs only TLS handshakes, and it disappears with the process.
- The worker announces the SHA-256 fingerprint of its `SubjectPublicKeyInfo` to the control plane along
  with its address. The control plane stores it (`artifact_peers.tls_fingerprint`) and passes it to
  authorized readers with the peer list.
- The reader **pins** that fingerprint: the handshake succeeds only against the exact key the control
  plane recorded for that holder. A man in the middle, a port reused by another process, or a replaced
  peer fails before the request is sent, and therefore before anything leaks about the requested
  artifact.
- Encryption is **not** what makes the bytes correct: every chunk is still verified against the signed
  manifest. TLS provides confidentiality and peer authentication, nothing more.

Libraries: `rustls` 0.23 (raw keys on both client and server), `tokio-rustls`, `hyper` — all already in
the dependency tree. No additional network stack, in line with ADR-0012.

## Decision — NAT traversal

What is implemented and verified:

- **IPv6 first**: IPv6 has no NAT, so there is nothing to traverse. By default the chunk server listens
  on `[::]:0` (IPv6 and, on a typical Linux system, IPv4; automatic fallback to IPv4 when IPv6 is
  unavailable), and a **global** IPv6 address is offered before any port-mapping request. Loopback,
  link-local, unique-local, documentation and IPv4-mapped (`::ffff:…`) addresses are never announced.
  The home router's firewall may block inbound traffic: the control plane's dial-back decides, and the
  worker then moves on to the next candidate.

- **Candidate list**: the worker offers its addresses in the order `configured`, `ipv6`, `nat-pmp`,
  `upnp`, `bound`, `observed`, and keeps the first one the control plane accepts after dialing it back.
  An unreachable address no longer blocks sharing: it is simply discarded.
- **Observed address**: the control plane returns the source address it sees (`/v1/worker/me` and the
  announce response). A worker that knows of no reachable address announces `observed_ip:local_port`.
  This is enough behind a NAT that preserves ports (endpoint-independent mapping) or when a port
  forward exists.
- **NAT-PMP (RFC 6886)**: the worker asks the default gateway for a port mapping (UDP 5351) and announces
  the external address it obtains; it renews at half the granted lifetime. The client is tested against a
  gateway that genuinely speaks the protocol (`tests/natpmp.rs`: full UDP exchange, gateway error code,
  bounded silence). **Never tested against a real router: none was available that supports it.**
- **Dial-back (`dial_back`)**: before recording an announcement, the control plane checks that the address
  accepts a TCP connection (bounded, without writing or reading). An unreachable contributor is not
  announced, so readers waste no time and the registry holds no dead addresses. This outbound connection
  follows the same address policy as announcements (private addresses refused in public mode), which
  limits the use of the server as a scanner.

- **UPnP-IGD**: SSDP discovery restricted to the default gateway (a response from another device on the
  network is ignored, and the description URL must be on the gateway), a bounded read of the description,
  then `AddPortMapping` **with a lease**, a re-read of the router's table
  (`GetSpecificPortMappingEntry`) to check that the mapping really exists, renewal at half-lease, and
  `DeletePortMapping` when sharing stops.

### Measurement on a real router (2026-09-17)

Development network gateway: a consumer ISP router, `192.168.1.1`.
Command: `gg-worker nat-probe --map 47654 --lease 60`.

| Mechanism | Measured result |
|---|---|
| NAT-PMP (RFC 6886, UDP 5351) | **no response** (two attempts, 2 s each) |
| PCP (RFC 6887, same port, version 2) | **no response** |
| UPnP-IGD (SSDP + SOAP) | **works**: `urn:schemas-upnp-org:service:WANIPConnection:1` discovered, external address read, TCP mapping created with a 60 s lease, **confirmed by the router's table**, then deleted and confirmed absent |

Conclusion: NAT-PMP alone would have left this contributor unable to share. UPnP-IGD is therefore
implemented and tried after NAT-PMP. The test mapping was removed immediately; no port was left open.

What is **not** implemented, and why:

- **Hole punching between two NATed peers**: it requires a UDP transport (QUIC), a synchronized rendezvous
  between the two peers and STUN discovery; over TCP the success rate is low and depends on the NAT type.
  That is a change of transport, and therefore a separate ADR and phase. Two workers behind restrictive
  NATs keep exchanging data through the control plane: the HTTP path is not degraded.
- **PCP (RFC 6887)**: not implemented (no gateway available here offers it, so it could not be verified).
  It would be added along the lines of the NAT-PMP client, testable against a test gateway.
- **Relays (TURN)**: out of scope; the control plane already plays that role, and better (durable source,
  authorization, accounting).

## Consequences

- Sharing works only between reachable peers (local network, machines with port forwarding, cooperative
  NAT via NAT-PMP). Elsewhere, distribution falls back entirely to the control plane, with no visible
  failure for the computation.
- The `ip:port` of a sharing contributor remains visible to workers authorized for the same artifact;
  encryption does not change that (see docs/THREAT_MODEL.md).
- A peer that loses its transport key (restart) gets a new fingerprint on its next announcement; readers
  holding the old one fail the handshake and fall back to another source. Announcements expire anyway
  (`p2p_peer_ttl_secs`).
