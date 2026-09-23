# ADR-0011 — Collective training of one model (`mlp.grad.v1` + coordinator)

Status: accepted and implemented (2026-09-16), phase P4 — see docs/STATUS.md for measurements.

## Context

P2 distributes independent trainings. P4 splits **a single** training: the coordinator holds the official
state (weights + optimizer), each task computes the gradient of an explicit micro-batch from a specific
checkpoint, and the coordinator aggregates once and atomically publishes the new checkpoint. There is no
silent switch to "local updates + averaging" (a different algorithm, outside the scope of this ADR).

## Decision

- **Compute type `mlp.grad.v1`** (catalog v1): inputs `w1,b1,w2,b2` (checkpoint, GGT1 artifacts),
  `train_x`, `train_y`; parameters `d_input,d_hidden,d_output`, `round_seed`, `example_start`,
  `example_count`, `total_examples`; the micro-batch is the slice `[example_start, example_start+count)`
  of the index stream `mlp.grad.v1/order`(round_seed) (concatenated Fisher–Yates permutations, as in P2).
  Outputs: `g_w1,g_b1,g_w2,g_b2` (the **mean** MSE gradient over the micro-batch) and `grad_meta.json`
  (`n_examples`, `loss`, profile). Verification: CPU replay of the gradient (a computation with no
  trajectory: a single step), compared within a tolerance; the GPU profile is admitted only after measurement.
- **Coordinator** (`gg-control::collective`): table `collective_runs` (config, current step, hashes of
  the current checkpoint and of the optimizer state) and `collective_rounds` (step, the round's tasks,
  expected slices, state `open|aggregated|failed`). A round creates `tasks_per_round` `mlp.grad.v1` tasks
  over disjoint slices that cover exactly `batch_per_round` examples. When all of them are `accepted`, one
  transaction: checks coverage (Σ n_i = batch, no duplicated slice), aggregates
  `g = Σ n_i·g_i / Σ n_i` (explicit weighting by number of examples; each g_i is a mean, and the loss is a
  mean), applies **a single** optimizer update (SGD or Adam, with state persisted as `opt_*.ggt`
  artifacts), publishes the new checkpoint (immutable artifacts) and opens the next round. Aggregating a
  round is idempotent (a uniqueness constraint on the published checkpoint's `(run, step)`).
- **Late responses**: policy `discard-late-v1`: a task from a round that has already been aggregated (or
  has failed) receives its decision (audit, payment according to the schedule) but is never folded into a
  later round.
- **Failure**: a `failed` task fails the round, which is reissued (new tasks, same data slice, same
  checkpoint) up to `max_round_retries` times; after that the run becomes `failed`.
- **Equivalence**: a test compares the collective run with a single-process reference training that uses
  the same effective batch, the same order, the same precision and the same rules; the tolerance is
  documented (different summation orders).
- **Local cache**: artifacts are cached by hash on the worker (checkpoints are reused across rounds); the
  cache is never a source of authority (hashes come from the server).

## Consequences

- The server performs the aggregation and the update on CPU (Burn Flex); no GPU dependency.
- The number of synchronization points equals the number of rounds; the demo measures the time per round.

## Implementation (2026-09-16)

- `gg-compute::grad` (execution, verification, aggregation, `apply_step`, `reference_training`),
  migration `0003_collective.sql`, `gg-control::workflows::collective` (creation, rounds, a coordinator
  running every 2 s, `FOR UPDATE SKIP LOCKED` on open rounds), routes
  `POST /v1/researcher/projects/{id}/collective`, `GET|POST /v1/researcher/collective/{run}[/cancel]`,
  CLI `gg job collective|collective-status|collective-cancel|collective-check`, demo `--kind collective`.
- Deviations from the original text: the optimizer state is published as separate per-tensor artifacts
  (`w1..b2` for each of `opt_m` and `opt_v`) referenced from the checkpoint JSON, rather than as named
  `opt_*.ggt` files; a `cancelled` task (canceled job) fails the round just like a `failed` task.
- Lesson learned: the validator downloads 6 inputs + 5 outputs per task; with no cache and no retry on
  `429`, the first demo stalled a verification until its lease expired. Hence the hash-keyed cache (worker
  and validator), bounded retries, the ×10 quota for compute sessions and explicit release
  (`POST /v1/validator/verifications/{id}/release`).
