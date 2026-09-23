# Functional specification — GPU Goblin

Version: 0.1 (phase P1). Terms in bold are domain objects modeled in `gg-core`.

## 1. Roles

- **Researcher**: an authenticated identity (ORCID from P3; a development token in P1). Authentication
  grants no rights by itself: a **ResearcherAdmission** (`pending`, `approved`, `suspended`, `rejected`)
  and a **ProjectMembership** (role `owner`, `member`, `viewer`) are required to act.
- **Worker** (contributor): a technical Ed25519 identity independent of ORCID (**WorkerIdentity**),
  authorized by an operator during the pilot. It declares an unattested **ExecutionProfile**.
- **Validator**: an Ed25519 identity administered separately; a worker key cannot declare itself a validator.
- **Operator/admin**: holds an administration token; approves admissions, allocates budgets, authorizes keys.

## 2. Objects

| Object | Description |
|---|---|
| **Project** | Container for authorization and budget. State `active`/`suspended`/`closed`. |
| **Experiment** | Scientific grouping of jobs (a hyperparameter grid in P2). |
| **Job** | A researcher's submission; split into tasks. State derived from its tasks. |
| **TaskSpec** | Immutable, versioned, hashed request (`spec_hash`); contains the instance identifiers and a scientific **definition** hashed separately (`definition_hash`). |
| **TaskAttempt** | An attempt assigned to a worker, with a role (`primary`, `replica`, `audit`) and a monotonic generation. |
| **Lease** | An attempt's lease: server-side expiry, renewable up to a maximum duration. |
| **Artifact** | Immutable object addressed by SHA-256: size, type, schema, visibility, license, retention. |
| **ResultSubmission** | Result signed by the worker agent, referencing the assignment, the specification and the artifacts. |
| **VerificationDecision** | Decision signed by a validator: method, version, checks, tolerances, outcome. |
| **ContributionReceipt** | Receipt signed by the server linking project, experiment, task, attempt, role, inputs, result, decision, beneficiary and amount. |
| **BudgetReservation** | Credit reservation tied to an attempt (`reserved`, `committed`, `released`). |
| **LedgerEntry** | Append-only accounting entry, balanced per transaction. |
| **RewardBatch** | Batch of rewards committed by a Merkle tree (phase P7). |
| **AuditEvent** | Audit log of authority actions (admission, budget, keys, cancellation, decisions). |

A **contribution** is an accepted attempt for an authorized role, not the mere existence of a result.

## 3. State machines

Notation: `state --(event | authority | condition)--> state`. See `crates/gg-core/src/state.rs`, which is
the executable, tested source of truth.

### Task

```
queued    --(assign     | server, on worker claim | project active, kind in catalog, budget reserved)--> assigned
assigned  --(submit     | worker, lease valid, generation == current)--> submitted
assigned  --(lease_lost | server reaper or worker abandon)--> queued   (attempts < max)  | failed (otherwise)
submitted --(verify     | validator)--> accepted | rejected | inconclusive
rejected  --(retry      | server policy)--> queued (attempts < max) | failed
inconclusive --(retry   | server policy)--> queued (attempts < max) | failed   (never implicitly accepted)
{queued, assigned, submitted} --(cancel | researcher(owner/member) or admin)--> cancelled
```

Terminal states: `accepted`, `failed`, `cancelled`. A `cancelled` task receives no further assignment;
a late submission on a canceled task is refused (409) and logged. A worker whose attempt on a task was
**rejected** by a validator is never assigned that task again (expired or abandoned attempts do not
trigger this exclusion).

### Attempt

```
active --(submit | worker, lease valid)--> submitted
active --(expire | server clock)--> expired
active --(abandon | worker)--> abandoned
active --(cancel | task cancelled)--> superseded
submitted --(decision)--> accepted | rejected | inconclusive
submitted --(cancel)--> superseded (any decision is recorded but pays nothing)
```

### Verification (per submission)

`pending --(claim | validator, lease)--> in_progress --(decide)--> accepted|rejected|inconclusive`;
`in_progress --(lease expiry)--> pending`.

### Reward

`none --(decision accepted)--> earned` (single write) `--(P7)--> batched --> settled`.
A payment incident never changes the scientific state.

### Job

Derived: `queued` if no task is active, `running` if a task is assigned/submitted, `succeeded` if all are
accepted, `failed` if a task is `failed`, `cancelled` if canceled.

## 4. Invariants (mandatory, tested)

| # | Invariant | Mechanism | Test |
|---|---|---|---|
| I1 | No work without an approved project, a catalog compute type and a reserved budget | claim transaction: `FOR UPDATE SKIP LOCKED` + budget lock + `CHECK (available >= 0)` | `gg-storage` tests `claim_*` |
| I2 | No acceptance based on declared metrics | the validator recomputes and compares; declared metrics are stored as "declared" | `gg-compute` tests `verify_*`, e2e corrupted result |
| I3 | An expired/closed attempt is never reactivated | monotonic `generation` in `tasks`; a submission checks `attempt.state = active AND lease valid AND generation` | `gg-storage` `late_submission_rejected` |
| I4 | A logical result changes the canonical state once | single `submitted` transition per task/generation; `UNIQUE(task_id, generation)` constraint on submissions | same |
| I5 | One reward = one effective write | `UNIQUE(reward_key)` on `ledger_transactions`; replay → same response | `gg-ledger` `reward_idempotent` |
| I6 | Credits never reserved twice; concurrency preserved | locked budget row, `CHECK`, reservations tied to a single attempt | `gg-ledger` `concurrent_reservations` |
| I7 | An accepted object is referenced only after durable storage + integrity check; bounded exception: an intermediate activation of a staged round circulates before its durable copy exists, and nothing is verified, aggregated, published or paid before that copy (ADR-0026) | temporary write + fsync + rename + SHA-256 check before the `artifacts` insert | `gg-storage` `object_store_*` |
| I8 | A canceled task gets no new assignment; races documented | cancellation in the same transaction as the state read; claim excludes `cancelled`; a decision on a `superseded` attempt grants no entitlement | `gg-storage` `cancel_races` |

## 5. P1 flow (vertical slice)

1. The admin creates an approved project and allocates credits to it (`gg admin ...`).
2. The researcher submits a `tensor.matmul.v1` job referencing two uploaded input artifacts.
3. The server validates the schema, estimates resources, computes the reward schedule and creates the tasks (`queued`).
4. An authorized worker authenticates (signed challenge) and asks for work; the server reserves budget + lease.
5. The worker runs the task through the separate executor, uploads the output and submits a signed result.
6. The validator claims the verification, recomputes the reference, compares within the tolerance and signs a decision.
7. The server applies the decision: budget commit, single reward, signed receipt, outbox events.
8. The researcher downloads the result; the worker exports its receipt; the CLI shows the summary.

## 5.2 P2 flow (independent trainings)

- `gg data gen-mlp` materializes a synthetic dataset (four GGT1 artifacts); `gg job submit --kind
  mlp.train.v1` or `gg job sweep` (a bounded grid of hidden × lr × seeds, ≤ 256 tasks in one job) submits
  **independent** trainings — this is not distributed training of a single model (P4).
- Each task produces final weights + `metrics.json` (bounded log, resumption); the validator replays the
  training (ADR-0008, docs/VALIDATION.md). `gg job compare` collects the accepted tasks and presents the
  comparison (evaluation loss per configuration) without promising convergence.

## 5.2a Data rule

Only **synthetic or already public** data enters the platform (docs/DECISIONS.md, D4). The reason is
structural: the owner of the machine doing the computation sees what it processes, and no
confidentiality can be enforced against them. The rule is published in plain text by `GET /v1/info`, and
a project exists only after it has been explicitly accepted, which is recorded on the project and in the
audit log.

## 5.4 P4 flow (collective training, ADR-0011)

- `gg job collective` creates a run: the server materializes the initial checkpoint (protocol weights,
  seed) and opens round 0 (a job of `tasks_per_round` `mlp.grad.v1` tasks over disjoint slices). Workers
  and the validator handle these tasks like any other (receipts, ledger, reassignment).
- The coordinator (a server loop) aggregates a round once all its tasks are `accepted`, applies a step,
  publishes the checkpoint `(run, step+1)` and opens the next round; a failed/canceled round is reissued;
  late results are refused. `gg job collective-status` shows rounds and checkpoints;
  `gg job collective-check` recomputes the run in a single process and measures the difference.
- This flow does not promise convergence: it guarantees that distribution does not alter the algorithm.

## 5.5 P5 flow (peer-to-peer distribution)

- A worker with an assigned task can request an input's **signed chunk manifest**, the bounded list of
  holders, and a signed **read ticket** bound to that artifact and to its key.
- It downloads chunk by chunk: each chunk is compared with the manifest **before** being written, a
  lying peer is excluded, and a missing chunk is requested from the control plane. The full hash is
  re-verified before the executor sees the file.
- A holder serves only artifacts from its cache, only on presentation of a valid server ticket and a
  request signed by the ticket's key, within local limits (connections, budget). Sharing is disabled by
  default and can be turned off on the server side (`p2p_enabled`).
- Peer-to-peer carries no leases, no decisions and no authority: only immutable bytes.

## 5.6 P6 flow (federated validation)

- The operator registers administrative entities (`gg admin operator-create`) and attaches each
  validator to one of them. Two keys belonging to the same operator never verify the same submission.
- A share of submissions is audited by a different operator, drawn after the result is recorded, using a
  server secret. Payment happens only if every verification accepts.
- A disagreement puts the task in `disputed`: nothing is paid, nothing is penalized, and the reservation
  is kept. An operator settles it (`gg admin dispute-resolve`) on an outcome signed by a validator.
- The signed receipt cites every recorded decision: an audited payment can be verified offline as backed
  by two distinct operators.

## 5.7 P7 flow (deferred settlement)

- A contributor declares a payout address (`gg worker payout`). Without one, their contributions are
  never committed: the platform does not choose who gets paid.
- An operator prepares a batch (`gg admin batch-create`, `--dry-run` to preview without writing): the
  accepted receipts not yet committed become leaves, sorted and hashed, and the batch receives a root.
- `gg admin batch-export` produces what a beneficiary presents to the contract: its leaf and its proof.
- Settlement follows its own state machine (`batch-settle`: `submitted`, `observed`, `finalized`,
  `reconcile`). An observation reports the root actually seen; if it differs, the batch goes to
  reconciliation. A transaction hash settles nothing on its own.
- The contract pays the committed beneficiary, once per obligation. None of this proves that a
  computation took place: it is the signed receipt, referenced by the leaf, that attests acceptance.

## 5.3 P3 flow (pilot)

- Researcher: `/portal` → "Sign in with ORCID" → admission `pending` → operator approval →
  API token from the portal → CLI. Tokens and sessions can be revoked (`gg admin`).
- Contributor: `gg-worker doctor` measures the sandbox; `run` refuses to start in public mode if a
  protection is missing.

## 6. Out of scope at this stage

Real ORCID untested (no credentials), P2P (P5), collective training (P4), contracts (P7).
