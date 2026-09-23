# ADR-0004 — PostgreSQL as the task queue, with no broker

Status: accepted (2026-09-16)

## Decision

Tasks, leases, reservations and the outbox are PostgreSQL tables. Assignment uses
`SELECT ... FOR UPDATE SKIP LOCKED LIMIT 1` in a short transaction that also reserves the budget and
creates the lease. Heartbeats and expirations use the server's `now()`. No broker (NATS/Redis) is
required; one will be added only if measurements show the need.

## Consequences

- A single transactional source of truth; invariants I1, I3, I5, I6 and I8 are expressed as SQL constraints.
- The worker's long poll is a bounded server-side loop (≤ 30 s) that queries the database at a fixed rate.
