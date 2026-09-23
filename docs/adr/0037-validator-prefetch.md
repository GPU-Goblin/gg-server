# ADR-0037 — Validators prefetch the inputs of tasks being computed

Status: accepted (2026-09-25).

## Context

After ADR-0036, a SmolLM2-135M round on the development machine took about 12.8 s:

| Stage | Time |
|---|---|
| Worker | about 5 s |
| Validator | 3.7 s, of which 2.06 s downloading |
| Server step | 3.1 s |

The validator's download was the new checkpoint (540 MB in f32) and the worker's gradient. Both
came after the submission, one after another.

The checkpoint cannot be kept from one round to the next: Adam changes every weight, so every
round has a new one. But it is known from the moment the round opens, while the worker is still
computing. Until now the server let a validator read an artifact only once it held the
verification, so nothing could be fetched earlier.

## Decision

- **Tasks ahead of a validator.** `POST /validator/upcoming` lists the inputs of tasks being
  computed or awaiting verification (`assigned`, `submitted`).
  - Only tasks of kinds the validator's profile may verify are listed.
  - Only inputs stored durably are listed.
  - The list goes largest first, at most 16 artifacts from the 64 most recent tasks. A partial
    index keeps the query as small as the work in flight.
- **The read rule.** A validator may read such an input, in addition to the inputs and outputs of
  verifications it holds. Once a task is decided, its inputs are neither announced nor readable.
  Workers' outputs are never readable in advance: the gradient still comes after the submission.
- **Prefetching.** The validator runs a prefetcher beside its loop (`prefetch = true` by default).
  - Every 500 ms it asks what is ahead and downloads what its cache lacks and can hold.
  - A lock per artifact is shared with verifications. A verification that needs an artifact being
    prefetched waits for it instead of downloading it twice.
  - The cache stays what it was: every use re-hashes the copy.
- **Parallel downloads.** A verification fetches its inputs and outputs at once, no longer one
  after the other.

## Consequences

**Measured on the grid.** SmolLM2-135M, bf16 gradients, 1 CUDA worker and 1 CUDA validator on one
machine:

| | Before | After |
|---|---|---|
| Validator's download | 2.06 s | 0.92 s, every input from the cache |
| Round | 12.1–12.9 s | 11.4–12.7 s |

- **What remains of the validator's download:** the worker's 270 MB gradient, and the re-hash of
  the cached checkpoint.
- **Why the round gains less than the download.** On one disk, the prefetch competes with the
  worker's own fetch of the same checkpoint (2.9–3.5 s). On separate machines they do not share a
  disk.

**What a validator learns earlier.** It sees the inputs of tasks in flight before holding their
verification: checkpoints, micro-batches, descriptions. A validator is an administered operator
and would see these inputs when verifying anyway. What it does not get earlier is any worker's
result. THREAT_MODEL.md records it.

**Test.** `a_validator_may_prefetch_the_inputs_of_tasks_being_computed` (`gg-control --test
e2e`) checks the whole life of an announcement:

| Task state | What the validator sees |
|---|---|
| Queued | Nothing announced |
| Being computed | Both inputs announced (largest first) and readable |
| Any state, validator whose profile may not verify the kind | Nothing |
| Submitted | Still announced |
| Decided | Neither announced nor readable |

**Not done.**

- The worker's own checkpoint fetch. It cannot start before the round opens, and it is now the
  largest piece of a round on this machine.
- Sending the gradient to the validator directly from the worker (P2P). The validator has no P2P
  client.
