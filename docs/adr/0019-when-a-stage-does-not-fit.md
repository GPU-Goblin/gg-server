# ADR-0019 — When a stage does not fit on a GPU: four levers before splitting the operator

Status: accepted (2026-09-17), phase P10. Complements ADR-0017 and ADR-0018.

## Context

ADR-0017 cuts the graph into stages that each fit in a contributor's memory. One case remains that no
cut can solve: **a single operator that overflows on its own**. The planner used to stop at "this node
is too large", without saying what to do.

There are two causes, and they call for different remedies:

- **activations**: a large micro-batch, a long sequence, a large output vocabulary;
- **parameters**: an embedding table or an output projection of several gigabytes, multiplied by four
  because Adam keeps the gradient and two moments.

## Decision

**Four levers, measured and proposed by the planner, before considering distributing an operator
across several peers.** A rejection names the node, says where its memory goes, and lists what would
make it fit, marking which options are enough on their own.

### 1. Shrink the micro-batch

Acts only on activations, but immediately and at no extra compute cost. Cost: more micro-batches, hence
more synchronizations.

### 2. Recompute activations during the backward pass

A stage keeps only its input boundary, its output boundary and its largest node, instead of all of its
activations. Cost: roughly a third more compute.

**Does not help on an isolated node**: on its own, a node has nothing it can drop. The planner therefore
only proposes it when it actually reduces the memory of the offending node — a remedy that changes
nothing is not listed.

### 3. Freeze the base, train only adapters

A parameter's memory factor goes from `4` (value, gradient, two moments) to `1 + 3·t`, where `t` is the
trained fraction. At 1 %, that is `1.03`: a reduction by nearly four. Synchronization shrinks in the
same proportion, since only trained parameters are exchanged. Cost: the base model is no longer
updated.

### 4. Offload parameters and optimizer state to host memory

The GPU keeps only the working window of the largest node; the rest lives in RAM and crosses PCIe at
every micro-batch.

**This is the decisive lever on a volunteer network, for a reason that is not obvious: PCIe is roughly
a thousand times faster than a home Internet connection.** A PCIe 4.0 ×16 link sustains 16 GB/s,
versus 37 MB/s for a 300 Mbit/s uplink. What would be prohibitive in a datacenter, where the internal
network is faster than PCIe, is almost free here. Host memory is also cheap: 64 GiB is common on a
gaming machine.

### A fifth lever, found by measuring: tile the matrix in the description

**An operator whose weight exceeds the card does not need a new engine: it can be tiled in the
description itself.** A product `y = x·W` with `W` of shape `[K, N]` can be written as `T` products over
column tiles `W_1..W_T`, joined back together by `Concat`. It is the same computation, expressed with the
profile's existing operators, and segmented execution with offloading does the rest: each tile is only
moved to the device for the duration of its own product.

Measured (`crates/gg-compute/tests/graph_tiling.rs`):

| Tiles | Peak parameters | Loss | Gradient |
|---|---|---|---|
| 1 | the whole weight | reference | reference |
| 2 | 1/2 | identical | identical to 1e-6 |
| 4 | 1/4 | identical | identical |
| 8 | 1/8 | identical | identical |
| 16 | 1/16 | identical | identical |
| 32 | 1/32 | identical | identical |
| 64 | **1/64** | identical | identical |

The division is exact, and the gradient of the rejoined tiles equals that of the whole weight.

**And it is not expensive at realistic sizes**, contrary to intuition
(`cargo run -p gg-compute --release --features gpu --example tiling_scale`):

| Weight | Batch | 1 tile | 4 | 16 | 64 |
|---|---|---|---|---|---|
| 64 × 256 (0.1 MiB) | 8 | 1.00× | 2.79× | 7.67× | 22.35× |
| 512 × 2048 (4 MiB) | 32 | 1.00× | 0.68× | 1.09× | 2.93× |
| 2048 × 8192 (64 MiB) | 64 | 1.00× | 0.97× | 0.72× | **0.68×** |

On an RTX 5060 Ti, tiling a 64 MiB weight into 64 tiles is **faster** than processing it whole. The
per-call overhead only dominates on toy matrices; as soon as tiles have a useful size it disappears,
and memory locality even makes tiling advantageous. The CPU shows the same trend (0.59× at 64 tiles).

**Consequence: the system's bound is no longer the largest weight, it is the largest activation.**
`Concat` materializes the whole rejoined output, and no tiling goes below it. This is measured, and it
leads back to the first lever: shrinking the micro-batch.

Two practical limits, stated rather than discovered: `Concat` accepts at most 8 inputs in the profile,
so beyond 8 tiles the joins must be chained (done and measured up to 64); and tiling is done by hand in
the description — no tool generates it.

### Only then: distribute an operator across several peers

Splitting a matrix product by columns requires **duplicating the input activation to every part**. On
the target links, this multiplies traffic by the number of parts for a linear memory gain: the worst
ratio of the five. **Not implemented, and deliberately last.** The case that might justify it one day
is the output projection over a very large vocabulary, where each part receives a narrow activation
and produces a slice of the logits.

## What the measurements give

`gg job template transformer --layers 2 --dim 8192 --heads 64 --seq 2048 --vocab 256000`, i.e. 5.8
billion parameters and 21.6 GiB, on a 12 GiB card.

**Rejection, with its remedies** (`--batch 4`):

```
no plan: node 'h0' alone needs 31.50 GiB, the machine offers 12.00 GiB
         (31.25 GiB of parameters and optimizer state, 256.0 MiB of activations for a batch of 4)
  [helps]  divide the micro-batch by 4 → 31.31 GiB (more micro-batches, hence more synchronizations)
  [enough] train only adapters → 8.30 GiB (the base model is no longer updated)
  [enough] offload to host memory (31.25 GiB needed) → 256.0 MiB (4.19 s of PCIe per micro-batch)
```

Each lever applied moves the bottleneck, and the planner names the next one: after offloading and
adapters, `seqlogits` (the logits over 256,000 words) overflows, and the remedy is the micro-batch.

**Result once all four levers are applied** (`--batch 1 --recompute --trainable-fraction 0.01
--host-offload-gib 64`), on 64 contributors on fiber:

| | |
|---|---|
| Stages | **1** — the model now fits on a single machine |
| GPU memory | 10.00 GiB out of 12 available |
| Host memory | 22.27 GiB |
| PCIe | 2.99 s per micro-batch, i.e. 33 % of the pace |
| Duration | **2.3 h** |
| One datacenter GPU | 1.7 h |

**The most important result is not the duration, it is the number of stages.** The levers brought a
5.8-billion-parameter model down to a single 12 GiB card. There is no cut anymore, hence no hand-off,
no pipeline fragility, and a verification that once again covers all of a peer's work. **The best split
is still the one you avoid.**

## Consequences

- The order of questions is fixed: shrink the micro-batch, recompute, freeze, offload — and only if all
  of that fails, split into stages. ADR-0017 becomes the last resort, not the first.
- A planner rejection is now actionable: it names the node, the breakdown of its memory, and what would
  make it fit, with the price of each option.
- The cost model knows the host-GPU link (`--host-link-gbs`, 16 GB/s by default) and charges for
  offloading just as it charges for the network.

## How the executor implements them (delivered)

**Segmenting execution provides recomputation and offloading in one stroke**, without depending on any
feature of the autodiff engine:

1. **Forward pass without an autodiff tape**, on the internal engine. A tensor is freed as soon as its
   last consumer has run, and only segment boundaries are kept.
2. **Backward pass segment by segment, walking backwards.** Each segment is rebuilt from its saved
   input boundary, then seeded with the gradient of its outputs. The seed uses the identity
   `d((y·g).sum())/dy = g`: it is exact, and it only requires the engine to backpropagate from a
   scalar.
3. **Offloading comes naturally**: since each segment is rebuilt in isolation, its parameters are only
   moved to the device at that moment. The reference value stays in host memory, where it already
   lived.
4. **Freezing** does not require tracking a parameter: it receives no gradient, does not appear in the
   transmitted container, and the coordinator allocates no optimizer state for it. A `graph.grad.v1`
   therefore declares two distinct sizes, `checkpoint_bytes` (all parameters) and `gradient_bytes`
   (trained parameters only).

### What the measurements say about the result

| Check | Result |
|---|---|
| Does the gradient change? | no: maximum relative error **1.8e-5**, and it **decreases** with the number of segments (1.25e-5 at 100 segments), which rules out a backpropagation bug |
| Independent proof | finite differences on the segmented path with offloading, without ever comparing it to the direct path |
| Peak activations | measured by the executor, not estimated, and it drops when segmenting |
| Peak parameters | with offloading, the largest segment instead of the whole model |
| A frozen parameter | receives no gradient, is not transmitted, and keeps its value byte for byte after a complete distributed run |
| Freezing everything | rejected at validation ("nothing to learn"), not discovered at run time |

The 1.8e-5 difference is rounding: segmenting reassociates f32 sums. The tolerance of the
`graph.grad.v1` type (abs 1e-4, rel 1e-3) covers it comfortably, and the validator recomputes **with
the same levers**, because recomputing without them would require the very memory the worker did not
have.

## Consequences of the implementation

- **A very large network trains on a single GPU**, without adding anything to the engine: weights are
  tiled in the description, and peak parameter memory drops accordingly. The bound becomes the largest
  activation, which the micro-batch controls.
- The lever left to write is therefore narrower than announced: distributing an operator across
  several **peers** is only useful for parallelism, not for fitting in memory.
- What would make tiling convenient is a tool, not an engine: generating the tiles and joins from an
  untiled description.
- A researcher can freeze the base of their model and train only adapters: what is exchanged at each
  synchronization drops in the same proportion, and so does the coordinator's optimizer state.
