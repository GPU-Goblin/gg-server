# ADR-0036 — The server's step: optimizer state kept in memory, moments written behind the round

Status: accepted (2026-09-25).

## Context

Once gradients travelled in bf16 (ADR-0035), the largest piece of a SmolLM2-135M round was the
server's own optimizer step: 8.8 s of a 16.5 s round. Every round, for every trained shard, the
server read the weights and both Adam moments (1.6 GB, each hash-checked), averaged the tasks'
gradients and applied Adam on one core. It then wrote the same 1.6 GB back, fsynced, before the next
round could open.

Measured on the development machine (`optimizer step timed` in the server log):

| Part | Time |
|---|---|
| Read (weights, moments, gradients) | 2.7 s |
| Compute | 0.6 s |
| Store | 6.0 s |

The disk writes and fsyncs 540 MB in 1.44 s (375 MB/s), so writing 1.6 GB costs at least 4.3 s,
whatever the code does.

## Decision

1. **No copies, no serial work.**
   - A bundle is encoded from the tensors without copying them.
   - The write, hash and fsync of a stored object run on a blocking thread, with the hash beside the
     write.
   - Reads (with their hash check and decoding) run in parallel: weights, moments and every task's
     gradient.
   - The mean and the Adam step are spread across the machine's threads. Each element sees the same
     operations in the same order, so the result is bit for bit the same. A test compares them with
     the serial formulas.
2. **The state stays in memory between rounds** (`StepCache`, `step_cache_bytes`, 4 GiB by
   default, 0 disables it).
   - After a step, each trained shard's new weights and moments are kept, by run and shard.
   - The next step uses an entry only for the exact weights it was computed with (their content
     hash) at the expected optimizer step. Anything else is dropped and read from the checkpoint.
   - A run whose state does not fit is stepped as before. The least recently stored entry is evicted
     first.
3. **Only the weights are written before the round closes.**
   - The next round's workers need the weights, so they are stored durably and hash-checked before
     they are referenced, as every artifact is.
   - The moments of step *n* are written by a background task while the workers compute round
     *n + 1*.
   - The aggregation of round *n + 1* waits for that task. It then records the moments in the new
     checkpoint, with `opt_t` = *n* and `opt_lag = 1`: they are one step behind the weights.
   - The database therefore always references a consistent, durable (moments, step) pair, at most
     one step older than the weights. No artifact is referenced before it is durable.
4. **After a restart or an eviction,** the kept state is gone and the recorded moments lag by one
   step. The server rebuilds them exactly:
   - The round that produced the weights is recorded with the checkpoint (`from_round`).
   - Its accepted gradients are durable: the task outputs for a data-parallel round, and the partial
     gradients listed in the round's stage state for a staged one.
   - The moment update (`advance_moments_named`) is the Adam step without the weights, with the
     same arithmetic.
   - No task is recomputed and nothing is paid twice.
5. **The last round writes everything** before the run completes: a completed run's checkpoint
   has `opt_lag = 0`.

## Consequences

**Measured on the grid.** SmolLM2-135M, bf16 gradients, 1 CUDA worker, 1 CUDA validator, all on
one machine and one disk:

| | Server step | Round |
|---|---|---|
| Before | 9.3 s (read 2.7, compute 0.6, store 6.0) | 16.5–20 s |
| Copies and serial work removed (1) | 7 s (store 4.0, compute 0.25, read 0.8–3.7) | 12.3–16.5 s |
| State kept in memory (2–4) | 3.1 s (read 0.47, compute 0.28, store 2.4) | 12.1–12.9 s |

- The remaining round is:
  - the worker, about 5 s, including a checkpoint fetch slowed here by the moments being written to
    the same disk;
  - the validator, 3.7 s;
  - the server's weight write, 2.4 s.
- On separate machines, the background write no longer competes with the workers' fetch.

**What the tests show.**

- **End-to-end runs still match a single-process replay bit for bit:** data-parallel, sharded,
  staged, tiled, bf16 and f32.
- **Two tests drop the kept state every 20 ms during a run, as restarts would.** One is
  data-parallel, the other staged. Steps rebuild their moments from the recorded gradients, and
  the final checkpoint is still bit for bit the replay's.
- **One test runs with `step_cache_bytes = 0`.**

**Costs.**

- The server holds up to `step_cache_bytes` of state: 1.6 GB for SmolLM2.
- A checkpoint recorded during a run references moments one step behind its weights. Anyone
  resuming a run from its optimizer state reads `opt_lag`. The published weights are unaffected.
- After a restart, the first step of each run reads the previous moments and the round's gradients
  again, and writes the rebuilt moments: about what every step cost before.

**Not done.**

- **Opening the next round before the weights are durable.** It would need an exception to the
  artifact invariant, like ADR-0026's for activations. It is not proposed.
- **Serving the weights to workers from the server's memory.**
