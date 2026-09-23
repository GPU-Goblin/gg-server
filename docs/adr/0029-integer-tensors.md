# ADR-0029 — Integer tensors computed inside the graph

Status: accepted (2026-09-24). Profile `gg-ops/8`. Builds on ADR-0016 (published profile),
ADR-0025 (measured coverage) and ADR-0027 (`gg-ops/7`, which stopped on this item).

## Context

`gg-ops/7` carries f32 between nodes. Integers exist only as **inputs** (`"dtype": "i64"`: token
ids, class targets) consumed by `Gather` and by the cross-entropy loss. Everything that computes an
index from the data is rejected by name: `argmax`, the indices of `max(dim)`, `topk`, `sort`,
`gather`/`scatter` driven by a computed index — and with them the routing of every mixture of
experts (Mixtral, Qwen-MoE, OLMoE), the one corpus model still refused.

## Decision

### 1. A type per tensor, inferred by validation

Validation now infers a **type** for every tensor, as it infers its shape: `f32` or `i64`.

- The description keeps its vocabulary. `i64` is already the type of index inputs, it is what
  PyTorch and ONNX use, and an `i32` next to it would add a choice without adding anything. The
  type is **semantic**, not a storage width: the executor holds integers in the backend's integer
  type (i64 on the CPU reference, i32 on WGPU/Vulkan).
- **Every integer is in `[0, 2²⁴]`.** Computed indices are bounded by the axis they index, and an
  axis an index is computed over is bounded at 2²⁴ elements. This makes them exact in i32, and
  exact in the f32 that carries tensors in `GGT1` files (`gg job eval-model` can write an `argmax`).
- Each operator declares which types it accepts. A node that feeds an integer to an arithmetic
  operator (`MatMul`, `Add`, `Formula`, …) is **rejected at validation**, naming the tensor. Before,
  such a description failed at execution ("expects a float tensor"): no description that executed
  changes meaning.
- The loss prediction is f32.

Where integers come from:

| Operator | Output | Rule |
|---|---|---|
| `ArgMax`, `ArgMin` | i64, the axis reduced (`keepdims` as `ReduceMax`) | **first** index among ties; a NaN counts as the largest (PyTorch's convention) |
| `ArgTopK` | i64, the axis cut to `k` | the `k` largest (`largest = 1`) or smallest, in order; ties: **lower index first**; NaN largest |
| `ArgSort` | i64, same shape | **stable** permutation, ascending or `descending = 1`; ties keep their order |
| i64 inputs | as declared | unchanged |

Where they go: `Reshape`, `Transpose`, `Slice`, `Concat` and `Flatten` accept either type and keep
it; `Cast` turns an integer into f32 (exact, below 2²⁴); `Gather` (rows of a table, now of rank 2 to
4: an expert's weight matrix), `GatherElements` and `ScatterElements` consume them as indices.

### 2. Several outputs without a new format: values are a gather of the indices

`topk` and `sort` return values **and** indices. `Node.output` names one tensor; making it a list
would change the format for every description. Instead:

```json
{"op": "ArgTopK",        "inputs": ["scores"],        "output": "idx", "attrs": {"axis": 2, "k": 2}},
{"op": "GatherElements", "inputs": ["scores", "idx"], "output": "top", "attrs": {"axis": 2}}
```

The values are `GatherElements(x, indices)`. This is exactly PyTorch's derivative of `topk`,
`sort` and `max(dim)`: the gradient reaches **the selected positions** and nothing else, through
the backward pass of the gather. The gather costs `k` reads per line, next to a sort. `gg-graph/v1`
keeps one output per node; a description that uses no integer operator is untouched, byte for byte
and in meaning.

`torch.max(x, dim)` (values and indices) becomes `ArgMax` + `GatherElements`, like PyTorch, whose
gradient goes to the one index it returns; `torch.amax` stays `ReduceMax`, which shares it between
ties (ADR-0027).

### 3. Integers are constants for automatic differentiation

An integer tensor has no gradient: nothing is tracked through `ArgMax`, `ArgTopK`, `ArgSort` or
`Cast`, and `GatherElements`/`ScatterElements`/`Gather` differentiate with respect to their float
inputs only. The choice of an index is piecewise constant in the data: its derivative is zero where
it exists, which is what PyTorch does.

### 4. The same result on every backend

The choices are **computed so that they cannot depend on the engine**:

- `ArgMax`/`ArgMin` on the device: the extremum, the mask of the elements equal to it (or of the
  NaNs, when a line has one), then the **smallest index** of the mask — an integer minimum, exact
  and independent of the reduction order. Burn's own `argmax` makes no promise on ties.
- `ArgTopK`/`ArgSort` on the host, with Rust's stable sort and the explicit comparison above.
  Burn's sorts are unstable, and its GPU backend sorts on the host anyway; the executor reads the
  tensor back once per node (one synchronization), which the router of a mixture of experts,
  `[tokens, experts]`, makes small.

Given the same values, CPU and GPU therefore choose the same indices, byte for byte (tested on the
RTX 5060 Ti with ties and NaNs). **Given values that differ by rounding**, a near-tie can be decided
differently: a discrete choice amplifies a 1e-7 difference into a different expert. The validator
recomputes on the reference profile and compares the gradient; a flipped choice shows up as a
disagreement, never as a silent acceptance (and a disagreement goes to a human, never to a sanction,
CLAUDE.md). The measured frequency on the MoE models is in the GPU test.

### 5. The batch axis, and index ranges

- No integer operator acts along the batch axis (an `ArgMax` over the batch mixes examples).
- `GatherElements(x, idx)` and `ScatterElements(x, idx, src)`: same rank; the batch axis is present
  in all of them or in none; `idx` is at most `x` on the other axes (PyTorch's rule; the gather reads
  a prefix), `src` has the shape of `idx`, and for a scatter `idx` equals `x` outside the axis.
- `Gather` reads rows (axis 0) of a table that has **no batch axis** (a parameter, or a tensor
  computed from parameters alone): selecting along the batch would mix examples.
- **An index is never out of range, and never clamped.** Validation knows the range of every
  computed integer (`[0, n)` for an index over an axis of `n`, kept through the shape operators).
  - A range that does not fit the indexed axis is rejected at validation, naming both.
  - An index that comes from the data (an i64 input) has no known range: the executor checks it
    **when the micro-batch is loaded**, on the host, against every axis it will index, and fails
    with a named error (`'ids': value 70000 at position 12 is outside [0, 64000)`). This also covers
    `Gather` and the cross-entropy target, which trusted their inputs until now (the GPU clamps an
    out-of-range read silently).

### 6. Scatter: accumulate, or assign distinct positions

`ScatterElements` has `reduction = "add"` (`scatter_add`, `index_add`) or `"none"`
(`scatter`). An assignment with a **repeated** index depends on the order of the writes; it is
accepted only when validation knows the indices are distinct along the axis — the output of
`ArgTopK` or `ArgSort` along that same axis, as in the gating of a mixture of experts. It is then
computed as `x·(1 − mask) + Σ src`, exact, with the derivative `(1 − mask)` for `x` and a gather for
`src`. Any other assignment is rejected by name. `scatter_reduce` with `sum` and `mean` is written
by the driver with two accumulations (values and counts) and a `Formula`, exact because the counts
are integers; `amax`, `amin` and `prod` are rejected by name (Burn has no such scatter, and a
decomposition would need data-dependent selection).

### 7. Stages

An integer tensor never crosses a stage boundary: `stages::stages` rejects such bounds, and
`stages::partition` does not cut there. A task's outputs are therefore always gradients or float
activations, compared within tolerance as before. Should integers one day travel between machines,
they are to be compared **exactly**: an index is right or wrong.

### 8. Cost, memory, version

- FLOPs: `ArgMax` 3 per element read; `ArgTopK`/`ArgSort` `n·⌈log₂ n⌉` per line; `GatherElements`
  0 (a copy); `ScatterElements` one per index (two for an assignment); `Cast` one per element.
- Memory: an integer counts 8 bytes (an upper bound: i32 on the GPU); a gather keeps its index for
  the backward pass, an assignment its mask.
- `gg-ops/8`. `required_profile` is per use: 8 only for a description that uses an operator of
  this ADR, a `Gather` table of rank above 2, or an i64 input outside `Gather` and the loss.

## What the measurements establish

| Property | Evidence |
|---|---|
| Indices, exactly | `crates/gg-compute/tests/graph_ops8.rs::ties_and_nans_follow_the_rules`: first index among ties, NaN largest (the first NaN wins, for `ArgMax` as for `ArgMin`), top-k lower index first, stable sort both ways; the host sort against a reference on 60 lines of repeated values, along every axis |
| Values and derivatives | `graph_ops8.rs`: 9 graphs (top-k, smallest-k along a middle axis, sort, `max(dim)`, `ArgMin`+`Cast`, `scatter_add` and gating assignment at computed indices, integer `Slice`/`Concat`, routed experts gathered from a `[experts, out, in]` table) against an f64 definition: value ≤ 8.6e-8 relative, gradient against central finite differences ≤ 9.4e-8; bit-for-bit determinism; recomputation segments give the same bytes |
| No gradient through an index | `indices_carry_no_gradient_…`: a top-2 read as values and as `Cast` indices; the gradient is exactly the selected positions' |
| Named rejections | an integer into arithmetic, an integer prediction, along the batch, `k` out of range, a range that does not fit the indexed axis, an assignment without distinct positions, a scatter `mul`, shapes, a table carrying the batch, a stage bound across an integer |
| Indices from the data | `an_index_from_the_data_is_checked_at_loading`: token ids and cross-entropy classes out of range are named errors at loading |
| On the GPU (RTX 5060 Ti, Vulkan) | `tests/gpu.rs::graph_ops_on_gpu`: gradient deviation ≤ 6.0e-8 absolute, ≤ 2.3e-6 relative for the seven operators; `ints_on_gpu`: indices identical to the CPU's byte for byte, ties and NaNs included; 0 of 8192 tokens routed differently (2 of 16 experts, scores from a matrix product) — a flip, if any, is asserted to be a near-tie |
| Translations | `gg-python/tests/test_ops8.py`: 12 constructs within 1e-5 of PyTorch (argmax, `max(dim)` indices, topk, sort, argsort, gathers, scatters, `scatter_reduce` sum/mean, `repeat_interleave`, a router with gathered expert weights, an embedding at a computed index); 7 rejections named |
| Real models | the corpus goes from 21 of 22 to **26 of 26**: the hand-written top-2 gating and, through `batched_experts`, Mixtral, Qwen2-MoE, OLMoE and Qwen3-MoE compute the same function as PyTorch (≤ 3.0e-7) |
| Trains like PyTorch | `graph_ops8.rs::captured_mixtures_of_experts_train_like_pytorch`: a Mixtral and the hand-written mixture captured with PyTorch's weights; loss and gradient of every trained parameter, the router's included, within 4.4e-7 of PyTorch's; on the GPU (`moe_on_gpu`), the same expert choices and a gradient within 3e-8 of the CPU |

## Addendum (2026-09-24): descriptions that hold at any batch size

The first live run of a Mixtral on the grid failed in every task: `Reshape: 16384 elements to
[32, 32, 1]`. The driver decided which dimension of a reshape follows the batch by comparing it
with the capture batch; in a mixture of experts the leading dimension is `batch·T·k` (the tokens
times the experts per token), so the capture's `32` stayed a number and the description only ran
with 2 examples. The corpus and the fixtures were evaluated at the capture batch and never saw it.

- **Fix, in the driver:** the module is captured twice, at the example batch and at one more
  example, and the two descriptions are compared (`export._merge_batch`). A `Reshape` dimension
  proportional to the number of examples becomes `-1` (it must be the leading one); any other
  difference — an attribute, a constant, a parameter, the structure — is refused by name instead of
  producing a description tied to the capture batch.
- **Guard:** every corpus model and every `gg-ops/7`/`gg-ops/8` construct of the Python tests is
  captured at 2 examples and executed at 3 and 5, compared with PyTorch at each batch
  (`tools/coverage_corpus.py::verify`, `tests/test_ops7.py::run_rust`); the training fixtures are
  captured at 2 and their loss and gradients checked at 3.
- **What the guard found besides:** `Gelu` accepted `approximate = "tanh"` and computed the exact
  form (Gemma, GPT-2 through `gelu_pytorch_tanh`): Gemma departed from PyTorch by 1.01e-5 at some
  batches. The executor now computes the tanh form (`graph_ops8.rs::gelu_tanh_is_the_tanh_form`,
  GPU deviation 1.5e-8); such a description requires `gg-ops/8`, so no earlier worker computes the
  other function. Gemma is now within 3.1e-7 of PyTorch.
- **Live:** `tests/test_live.py::test_a_mixture_of_experts_trains_on_the_grid` (Mixtral, 4 experts,
  top-2, micro-batches of 32 after a capture at 2): loss 2.77 → 0.060, accuracy 6.0% → 98.2% in 300
  rounds.

## Consequences

- Top-k routing (Mixtral and its family through `transformers`' `batched_mm` experts, selected by
  `gpugoblin.batched_experts`, and the hand-written corpus case), `argmax` pooling and sorting are
  expressible, with PyTorch's derivative.
- `Gather` now refuses a table whose first axis is the batch (it would select among examples). No
  description of the corpus or the examples did so.
- The router's host sort costs one synchronization per MoE layer on the GPU.
- What stays outside: outputs whose **shape** depends on the data (`nonzero`, `torch.where` with one
  argument, boolean masking, `unique`) — which is what the *eager* Mixtral experts use; `one_hot`;
  integer arithmetic; `scatter_reduce` with `amax`/`amin`/`prod`.
