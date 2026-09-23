# ADR-0005 — Ed25519 identities, domain separation, network binding

Status: accepted (2026-09-16)

## Decision

- Ed25519 keys (`ed25519-dalek` 3.0, `verify_strict`) for the server, workers and validators; 32-byte
  secrets generated with `getrandom`, stored in `0600` files.
- Every signed or hashed message is prefixed with `gpu-goblin/v1\n<network>\n<domain>\n` (see
  [gg-core/docs/PROTOCOL.md](https://github.com/GPU-Goblin/gg-core/blob/main/docs/PROTOCOL.md)). An object's hash is the SHA-256 of the tagged message.
- Artifacts (raw bytes) are addressed by their untagged SHA-256.
- A worker or validator authenticates with a single-use signed challenge (`WorkerLoginV1`) bound to the
  network and to the action, which yields a short-lived session. The token payout address (P7) is a field
  separate from the technical key.

## Consequences

A valid signature is never an authorization: before acting, the server maps the key to an authorized
record (an authorized worker, an administered validator).
