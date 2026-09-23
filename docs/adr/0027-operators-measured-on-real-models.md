# ADR-0027 — Profile `gg-ops/7`: the operators real models were measured to lack

Status: accepted (2026-09-24). Profile `gg-ops/7`. Builds on ADR-0016 (published profile, what an
operator brings) and ADR-0025 (`Formula`, measured coverage).

## Context

The goal is to capture and train any PyTorch model — from ResNet to Qwen, LSTMs, ViT, speech
synthesis — not a list of chosen architectures. `gg-ops/6` translated about 52 of the 160 core ATen
operators. Two sources said which ones to add first:

- **the corpus** (`gg-python/tools/coverage_corpus.py`): T5 was rejected (relative position bias),
  a GNN too (`index_add`);
- **miniature `transformers` speech models**: both HiFi-GAN vocoders (SpeechT5, FastSpeech2)
  failed **only** on `aten.conv_transpose1d`, Wav2Vec2 **only** on `aten._weight_norm`.

The rest of the core ATen list was sorted into what fits the format as it is (f32 tensors between
nodes, static shapes, batch axis split between workers) and what does not.

## Decision

### 1. Eleven operators, each with everything ADR-0016 requires

| Operator | What it does | Implementation |
|---|---|---|
| `ConvTranspose2d` | transposed convolution, PyTorch kernel layout `[C_in, C_out/g, KH, KW]`; 1D is height 1 | Burn `conv_transpose2d` |
| `Pad` | `pads` in ONNX order, mode `constant` (value), `reflect`, `edge` | Burn `pad` |
| `Resize` | last axis (1D) or last two (2D), `nearest` or `linear`, `align_corners` | selections by index and weights computed as PyTorch does; not Burn's `interpolate`, whose GPU backend has no bilinear backward pass |
| `Flip` | reverses the listed axes | Burn `flip` |
| `ReduceSum` | sum along one axis | Burn `sum_dim` |
| `ReduceMax`, `ReduceMin` | extremum; the gradient is **shared equally between ties**, as `torch.amax` does | `m + Σ((x − m)·mask)/Σ mask`, `m` detached: the value is exactly `m`, the derivative `mask/Σ mask` |
| `ReduceProd` | product along one axis | pairwise products (`log₂ n` levels): exact derivative with zeros, unlike `exp(Σ log x)` |
| `CumSum` | inclusive cumulative sum | Burn `cumsum` |
| `IndexSelect` | slices listed in a **constant** `index` attribute | Burn `select` |
| `IndexAdd` | `x` + `src` accumulated at the positions of a **constant** `index` (`torch.index_add`) | Burn `select_assign(Add)`, one ordered pass per output line, on the GPU too |

None of them may act along the batch axis: the execution model splits the batch between workers,
and an operator that mixed examples would compute something else once split. This is refused at
validation, with the reason.

Constant indices live in the description (bounded at 2²⁰ per node, each checked against the
axis): a relative position table of 1024 × 1024 fits, and so does a fixed graph.

### 2. Fifteen functions in `Formula`

`tan`, `asin`, `acos`, `atan`, `sinh`, `cosh`, `asinh`, `acosh`, `atanh`, `atan2(y, x)`, `floor`,
`ceil`, `round`, `trunc`, `sign`. The last five are piecewise constant: **their derivative is zero
wherever it exists**, and undefined at the jumps; automatic differentiation returns zero there, as
PyTorch does. Nothing is invented: that is the derivative.

`round` is half to even, as PyTorch, **on every backend**. The GPU's native rounding (SPIR-V
`Round`) leaves ties to the implementation; the executor therefore writes it with `floor` and exact
comparisons (`x − floor(x)` is exact in f32). The GPU test checks the bytes on ties.

A `Formula` that uses one of these functions requires `gg-ops/7`; one that does not stays at
`gg-ops/6` (`graph::memory::required_profile`), so an older worker is still offered it.

### 3. Translations in the driver, from existing operators

Exact decompositions, checked against PyTorch like everything else:

- `_weight_norm(v, g, dim)`: `ReduceSum` of the squares over the other axes, then
  `Formula("a * (b / sqrt(c))")`; `g` and `v` stay the trained parameters;
- `var`, `std`, `var_mean` (any correction), `logsumexp`, `_log_softmax` (stable form, with
  `ReduceMax`), `linalg_vector_norm` (orders 1, 2, ∞), `F.normalize`;
- `repeat` (copies with `Concat`), `roll` (`Slice` + `Concat`), circular padding, negative padding
  (a crop, `Slice`);
- `remainder` and `fmod`, written `a − b·floor(a/b)` and `a − b·trunc(a/b)`: exact when the
  quotient is; otherwise within one unit in the last place of `a`. Their derivative is PyTorch's
  (`−floor(a/b)`, `−trunc(a/b)` with respect to `b`);
- `log2`, `log10`, `exp2` through `log` and `exp` (within two units in the last place), `frac`,
  `erfc`, `selu`, `celu` (and `elu` with its `scale`/`input_scale`, which the driver used to ignore),
  `softsign`, `tanhshrink`, `logsigmoid`, `hypot`, `logaddexp`, `lerp`, `addcmul`, `addcdiv`,
  `clamp` by tensors, `add`/`sub` with `alpha`, the division with a rounding mode, `floor_divide`;
- `index_select`, `x[:, idx]`, `gather` and `scatter_add` whose index is the broadcast of one
  constant vector, `index_add`, an embedding looked up with a constant, non-contiguous index (T5's
  relative position buckets, computed from constants only);
- `upsample_nearest{1,2}d`, `upsample_bilinear2d`, `upsample_linear1d`: when PyTorch passes a scale
  factor instead of a size, the driver checks that its source coordinates are the ones the executor
  computes from the sizes, and otherwise refuses;
- `where` whose constant condition keeps everything (T5's encoder mask).

A tensor computed from parameters alone has no batch axis: the driver knows it (the cone of nodes
that depend on an input), which lets `IndexSelect` read the rows of a table.

## What the measurements establish

| Property | Evidence |
|---|---|
| Value and derivative of each operator | `crates/gg-compute/tests/graph_ops7.rs`: 22 cases written from PyTorch's definition in f64, derivatives against central finite differences of that definition: value ≤ 3.1e-7 relative, gradient ≤ 3.6e-7; ties shared, zero in a product; named rejections (batch axis, indices, pads, sizes, modes); bit-for-bit determinism; FLOPs and memory; the memory estimate bounds the measurement |
| New `Formula` functions | `tests/graph_formula.rs`: six expressions against their hand-written f64 definition (value ≤ 2e-6, gradient ≤ 1e-5), half-to-even rounding and zero derivative on exact values |
| On the GPU (RTX 5060 Ti, Vulkan) | `tests/gpu.rs::deviation_per_operator`: maximum gradient deviation from the CPU, admitted at abs 1e-5 + rel 1e-3 — ConvTranspose2d 7.6e-6, Pad ≤ 1.2e-6, Resize ≤ 2e-6, Flip, ReduceSum/Max/Min/Prod, CumSum, IndexSelect, IndexAdd ≤ 6e-8; `formula_on_gpu::functions_of_gg_ops_7_match_the_cpu`: gradient within 2.9e-6, rounding functions identical byte for byte |
| Translations | `gg-python/tests/test_ops7.py`: 44 translated constructs, relative gap to PyTorch ≤ 6.5e-7 (`remainder`/`fmod`), ≤ 2.6e-7 for the others; 10 rejections named |
| Captured models **train** as in PyTorch | `tests/graph_ops7.rs::captured_speech_models_train_like_pytorch`: a SpeechT5 HiFi-GAN vocoder and a Wav2Vec2 encoder captured by the driver, with PyTorch's weights; loss and gradient of every trained parameter against PyTorch's (fixtures from `gg-python/tools/make_torch_fixtures.py`): relative gap ≤ 9.7e-7. On the GPU (`tests/gpu.rs::speech_on_gpu`): gradient within 4.0e-5 absolute of the CPU, accepted by a CPU validator |
| Real models | the corpus goes from **16 captured out of 18** to **21 out of 22**: T5 (3.9e-7), the GNN (8.3e-8), both HiFi-GAN vocoders (≤ 1.7e-8) and Wav2Vec2 (≤ 8.8e-7) compute the same function as PyTorch; the one rejection is a mixture of experts routed by `topk` |

## What stays outside, and the design it needs

Each item is rejected by name today; none is approximated.

1. **Integer tensors computed in the graph** — `argmax`/`argmin`, the indices of `max.dim`, `topk`,
   `sort`/`argsort`, `gather`/`scatter` by a computed index, a mixture of experts' routing, CLIP's
   pooling at `argmax(ids)`. The format carries f32 between nodes. What it needs:
   - a `dtype` per tensor in validation (`infer` returns a shape **and** a type), `i64` outputs for
     `ArgMax`, `TopK` (two outputs), and `Dyn` variants `I1`…`I4` in the executor;
   - operators consuming an index tensor: `GatherElements`, `ScatterElements(add)`, both with the
     same batch rule as the constant forms here;
   - an explicit rule that an integer tensor carries no gradient (a node with an integer output is
     a constant for automatic differentiation), and a validation that no float gradient is expected
     through it;
   - multiple outputs per node (`TopK` values and indices), which `Node.output` does not allow
     today.
   The value of `argmax` itself is well defined; its tie rule must be fixed (first index, as
   PyTorch) and checked on the GPU.
2. **Shapes that depend on the data** — `nonzero`, `unique`, boolean masking. The graph is static,
   and the memory estimate and the distribution plan depend on it. A bound declared in the
   description (a maximum length with a fill value) would be needed; this is a format change.
3. **Assignment by index** (`index_put`, `x[idx] = v`, `scatter` without reduction): with repeated
   indices the result depends on the order of writes. It needs integer tensors (1) and a rule for
   duplicates (last write, in index order) that the GPU respects.
4. **`upsample_nearest_exact`, bicubic, antialiased bilinear**: other coordinate rules, each one a
   `Resize` mode with its own oracle; not measured as blocking.
5. **Rank above 4** (`reflection_pad3d`, `conv3d`, video): `bounds::MAX_RANK` and `Dyn`.
