# ADR-0024 — Staged execution: a model larger than any single machine

Status: accepted (2026-09-23). Step D2 of the P11 plan (docs/ROADMAP.md). Implements ADR-0018 (except the
direct hand-off of activations between peers, see below) and builds on C1 to C4.

## Context

Until now, each task computed the gradient of the **whole model** on a share of the data: a model had
to fit on a contributor's machine, memory levers included (ADR-0019). Parameter offloading frees the
card, but the process still holds the whole model in host memory. To train what no single machine can
hold, the model itself has to be cut.

## Decision

### A stage is a contiguous run of nodes

The bounds `0 = b₀ < b₁ < … < b_K = n` are indices in the graph's execution order. Everything else
follows from them, deterministically, on the server, the worker and the validator (`graph::stages`):

- the stage's **parameters**: those its nodes consume;
- **inputs**: the tensors it consumes without producing them. These are datasets, or outputs of an
  earlier stage, not necessarily the immediately preceding one: a residual connection can skip over a
  stage;
- **outputs**: what it produces and a later stage reads, or the loss reads;
- the last stage computes the loss and receives the target.

The checkpoint is split **per stage**: shard `j` carries the parameters whose first use is in the same
stage. A stage task downloads only the shards of its own parameters.

### One task type: `graph.stage.v1`

A task is one stage, **forward pass or backward pass**, on a micro-batch the coordinator has already
selected:

- **forward**: inputs → outputs `out.k`;
- **backward**: the stage is **recomputed** from its inputs, then the gradients received from its
  consumers (`dout.k.c`, summed in decreasing stage order) are backpropagated. It returns the gradients
  of its parameters (`gradient.j`, per shard) and those of its inputs (`din.k`), which the coordinator
  hands to the stages that produced them.

This is ADR-0019's segmented execution, spread across machines. Verification is the same as for any
task: the validator recomputes the stage **from the same inputs**. A false activation is therefore
caught at the stage that produced it, before it is aggregated.

### The coordinator advances in waves

A round of a staged run goes through `2K` waves: the forward passes of stages 0 … K−1, then the
backward passes of K−1 … 0. Each wave is a job with one task per micro-batch, run in parallel. When a
wave is accepted, the coordinator records what it produced (`collective_rounds.stage_state`) and
composes the next one. After the last one, it aggregates **shard by shard** as in C2, applies Adam,
publishes the checkpoint and opens the next round. A failed task fails the round, which is reissued
from the first wave with the same checkpoint.

Each task carries its own memory requirement, that of its stage: the server only offers it to a worker
that announces enough (C1).

### The researcher writes nothing more

`POST /researcher/plan` proposes stages when no worker can hold the whole model, even with the memory
levers. It then cuts below the memory of the largest worker that accepts `graph.stage.v1`.
`Trainer.fit()` uses them on its own. `stage_bounds` or `stage_memory_bytes` can also be given when
creating the run.

## What the measurements establish

| Property | Evidence |
|---|---|
| Chaining the stages gives back the gradient | `tests/graph_stages.rs`: transformers (RoPE, learned positions) and a real Llama from `transformers`, 2, 3 and 5 stages: **identical byte for byte** to segmented execution with the same bounds |
| Bug found along the way | a boundary left as a **transposed view** in segmented execution made the next matrix product accumulate in a different order (difference 2.7e-7). It is now contiguous, as when it crosses the network |
| Partitioning under a memory limit | `stages::partition`; a stage's memory is an upper bound of the measurement (forward and backward pass); a node that does not fit on its own: named rejection |
| Stage tasks | `tests/graph_stage_task.rs`: a full step in six tasks gives back the chain computed in memory **bit for bit**; every task is accepted by the validator; tampered activation rejected; shard from another stage rejected; declared memory too small rejected before computing. Against segmented execution: same loss, gradients within 2.4e-7 (see below) |
| **On the server** | `tests/collective.rs::a_model_larger_than_every_worker_trains_in_stages`: three workers announce **a third** of the whole model's memory; the server partitions, has 48 tasks verified, and publishes **exactly** the weights of a single-process replay |
| On the real grid | see docs/STATUS.md (P11, D2) |

**Measured residual difference.** When a residual connection skips over a stage, the embedding gradient
can differ by one or two ulp from segmented execution on one machine (≤ 3.6e-7). This is a different
accumulation order in the backpropagation of `Gather`, which I have not pinned down more precisely. The
difference is far below the validator's tolerance (1e-4), and the distributed run remains **exactly
reproducible with itself** and with its replay.

## What is not done

- **Activations go through the control plane.** Each output is sent to the server, which keeps the
  reference copy. Since ADR-0026, the next stage fetches it from its producer when P5 is enabled;
  sending it to the server remains on the critical path (a human decision, ADR-0026).
- **A wave used to wait for the previous one's verification.** Lifted by ADR-0026: waves are
  speculative, and a round goes from 21.7 s to 4.6 s (3 validators). There is still no pipelining
  between micro-batches.
- **A matrix larger than one machine** fits in no stage: that is D3 (tile the matrix in the
  description, then distribute it).
- **Weights do not stay on the machines** from one round to the next (D1): each task downloads its
  stage's shards.
- **The full cost of an accepted result** with stages has not been measured again (D6): every stage is
  verified, so verification tracks compute, but wave latency adds to it.
