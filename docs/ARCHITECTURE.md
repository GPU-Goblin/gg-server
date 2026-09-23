# Architecture

## Overview

```
 researcher (gg CLI)         contributor (gg-worker agent)          validator (gg-verifier)
      |  HTTPS /v1/researcher      |  HTTPS /v1/worker                 |  HTTPS /v1/validator
      v                            v                                   v
 +---------------------------- gg-server (gg-control, Axum) ----------------------------+
 |  admission & authorization | scheduler (claim, leases, reaper) | receipts & ledger   |
 |  versioned API + OpenAPI   | transactional outbox              | objects (gg-storage)|
 +---------------------------------------------------------------------------------------+
      |  SQLx (short transactions)                   |  files addressed by SHA-256
      v                                              v
   PostgreSQL 17 (source of truth)              durable local store (later an object-store adapter)

 worker agent --(child process, typed args, cleared env)--> gg-executor --(Burn Flex / WGPU)--> bounded outputs
```

- A single modular backend: the API, scheduling and orchestration loops live in `gg-server`.
- The worker never talks to PostgreSQL: only HTTP(S), with a bounded long poll.
- The validator is a separate process with its own key; it recomputes and signs.
- No broker: the queue is PostgreSQL (`FOR UPDATE SKIP LOCKED`), see ADR-0004.
- Deferred settlement (ADR-0015): `gg-merkle` defines the commitment in one place, the CosmWasm contract
  `contracts/gg-rewards` distributes, and the control plane follows a separate state machine without ever
  talking to a chain.
- Peer-to-peer artifact distribution (ADR-0012): workers exchange chunks verified against a manifest
  signed by the server, which remains the only durable copy, the only tracker and the only authorization
  authority. The transport moves nothing but immutable bytes.

## Crate dependencies

```
gg-core <- gg-protocol <- gg-compute (Burn) <---------------- gg-worker, gg-verifier, gg-cli
   ^            ^    ^                                           ^
   |            |    +-- gg-client (reqwest, artifact cache) ----+
   |            +------- gg-storage (SQLx) <- gg-ledger <- gg-control (Axum, no Burn GPU)
   +-- gg-merkle (settlement commitment) <- gg-control, contracts/gg-rewards
xtask (local orchestration: database, checks, demonstrations)
```

`gg-control` depends on `gg-compute` only for the catalog (schema validation, estimation, reward
schedule), with the GPU features disabled; actual execution happens on the worker and validator side.

The client side of the control plane lives in `gg-client` (`gg_client::client::Api`: discovery,
challenge and session, verified and resumable download, streaming upload with backpressure;
`gg_client::cache`: the SHA-256 artifact cache). It is shared by `gg-worker`, `gg-verifier` and `gg-cli`.
A background upload can give way to more urgent traffic through the `YieldTo` trait, which the worker
implements with the chunks it is serving to its peers (ADR-0026).

## Repositories

The code is published as several repositories of the GPU-Goblin organization:

| Repository | Contents |
|---|---|
| `gg-core` | `gg-core`, `gg-protocol`, `gg-compute`, `gg-client`: the domain, the signed protocol, the computations and the client. Everything the other repositories share. |
| `gg-server` | `gg-control` (`gg-server`), `gg-storage`, `gg-ledger`, `gg-merkle`, `gg-verifier`, `gg-cli` (`gg`), `xtask`, `contracts/gg-rewards` |
| `gg-worker` | `gg-worker` and `gg-executor`: the contributor agent, the sandboxed executor, peer-to-peer transfers |
| `gg-python` | The `gpugoblin` Python package and PyTorch driver |
| `gg-console` | The operations console (planned) |
| `gg-web` | The public website (planned) |

## Transactions and write ordering

- **Artifact**: bytes → temporary file → fsync → atomic rename → `artifacts` row (hash, size).
  A row without a file is impossible by construction; a file without a row is an orphan to clean up
  (`gg admin gc` in P3).
- **Claim**: one transaction: select a compatible `queued` task with `SKIP LOCKED` → lock the budget row →
  reservation → attempt + lease → task `assigned` → ledger entries → outbox → commit. No computation,
  download or GPU work while a lock is held.
- **Submission**: check signature + lease + generation → `submissions` (unique per task/generation)
  → task/attempt `submitted` → `verifications` `pending` → outbox.
- **Decision**: validator signature → transitions → budget commit/release → reward
  (unique key) → signed receipt → outbox. Replaying the same decision: identical response, no writes.
- **Outbox**: events are read by an internal loop that logs them (no broker); redelivery is possible,
  and effects are idempotent thanks to uniqueness constraints.

## Worker and executor

The agent (`gg-worker`) holds the key, the sessions and the cache; it writes one **bundle** per attempt
(`task.cbor`, `inputs/<name>`, `outputs/`) and then launches `gg-executor run --task-dir … --engine …` with
an emptied environment, stdin closed and bounded outputs. The executor has neither a key nor access to
the server; it writes `outputs/report.json`. The agent checks each output (hash, size, limit), uploads,
signs and submits. Heartbeats are sent during execution; exceeding `max_duration_secs` → kill + abandon.
The executor is launched inside bubblewrap (namespaces, no network, read-only system, only the bundle
writable) and restricts itself (Landlock, seccomp, rlimits) before reading any input (ADR-0009).
`gg-worker doctor` runs an escape probe in the real configuration and refuses to start if a protection
is missing (public mode), or unless `dev_unsafe_no_isolation` is set (development, `sandbox = "none"`).

## Scheduling

Combined criteria: compatibility, budget, age, exclusion of workers already rejected on the task;
planned: cross-project fairness, cache, estimated duration, observed reliability. The capabilities
declared by a worker filter tasks but are not attested. Measured: 60 concurrent workers, ≈194 claims/s,
no double assignment (`gg-control/tests/claim_storm.rs`).

## Configuration

TOML plus environment variable overrides (`GG_*`), parsed with Serde. Durations are integers in
seconds. Dangerous modes (dev without isolation, simulated ORCID) are explicit and shown at startup.

## Observability

`tracing` with `job_id`, `task_id` and `attempt_id` fields (for correlation), with no scientific data or
secrets. Exportable metrics (P3) with bounded labels (kind, state, result), never identifiers.
