# ADR-0025 — Custom operators: `Formula`, description functions, measured coverage

Status: accepted (2026-09-23). Profile `gg-ops/6`. Builds on ADR-0016 (published profile) and ADR-0023
(`transformers` models as they are).

## Context

A researcher writing their own model ran into the profile as soon as they used a missing function:
`exp`, `softplus` or a cubic power. They also ran into it with a split (`chunk`, `split`) or a common
layer (`Conv1d`, `GroupNorm`, `LSTM`). The answer was correct, since it named the operator, but it
stopped everything. Two paths were ruled out from the start:

- **running the researcher's code** on contributors' machines. This goes against the threat model:
  nothing that crosses the network is code;
- **adding operators as requests come in**, without knowing which ones matter.

## Decision

### 1. Measure first: a corpus of real models

`gg-python/tools/coverage_corpus.py` captures 18 architectures with `gpugoblin.coverage()`, which lists
everything missing in a single pass:

- 14 miniature `transformers` models, including Llama, Qwen2, Mistral, Gemma, Phi-3, GPT-2, GPT-NeoX,
  BERT, DistilBERT, T5, ViT, ResNet, ConvNeXt, the Whisper encoder and Mamba;
- four hand-written classics, including an LSTM, a GNN and a Conv1d followed by a GroupNorm.

The 2026-09-23 measurement gave **3 models captured out of 18**. The blockers, ranked by number of
models blocked:

| Blocker | Models blocked |
|---|---|
| `scaled_dot_product_attention` with a mask | 5 |
| `chunk` | 3 |
| `conv1d` | 3 |
| `embedding` with constant indices | 2 |
| `select` | 2 |
| `softplus` | 2 |
| about fifteen others | 1 each |

Most blockers were **missing translations in the driver**, not missing operators in the executor.
`chunk` is written with `Slice`, `conv1d` with `Conv2d`, `select` with `Slice` and `Reshape`. Only one
need truly called for something new: element-wise functions.

### 2. `Formula`: an element-wise expression, inside the description

A `Formula` node carries an `expr` attribute, for example `"a * tanh(softplus(a)) + 0.1 * b"`, and one
to four inputs named `a`, `b`, `c`, `d`, broadcast as for `Add`. The grammar is **closed**:

- the operations `+ - * /`, unary minus and parentheses;
- a power with a **literal exponent** (`a^3`, `a^-0.5`);
- fifteen functions: exp, log, log1p, sqrt, rsqrt, abs, tanh, sigmoid, relu, sin, cos, erf, softplus,
  max, min.

It is bounded: 1024 bytes, 128 terms, 32 levels of nesting. It has no loops, no variables and no calls
outside the list. Every input must be used: an ignored input would receive no gradient, and the output
shape would no longer be the announced one.

The executor evaluates the expression with Burn operations. **The backward pass follows from automatic
differentiation**, on the CPU as on the GPU, with no extra code. Two execution choices are fixed by the
specification, so that the result does not depend on the engine:

- an integer power with absolute value ≤ 8 is computed by repeated multiplication. It stays exact for a
  negative base: `(-2)^3 = -8`, where `powf` would return `NaN`;
- `softplus` is computed in the stable form `relu(x) + log1p(exp(−|x|))`.

The cost is read off the expression:

- FLOPs per element, the sum of the terms (4 per transcendental function);
- memory: one output-sized tensor per intermediate step, which automatic differentiation may keep.

It enters the distribution plan like that of any other node.

### 3. Description functions: named subgraphs

```json
"functions": [{"name": "layer", "inputs": ["h", "w", "b"], "output": "a", "nodes": [...]}],
"nodes": [{"op": "fn:layer", "inputs": ["x", "w1", "b1"], "output": "h1"}, ...]
```

A function groups nodes under a name and is called like an operator, as many times as needed, each call
with its own inputs and parameters. It is **closed**: its body reads only its formal inputs and what it
produces itself, and parameters are passed to it. It only calls functions declared **before** it, hence
never itself. The number of functions is bounded (64), as are their inputs (32) and the total number of
nodes after inlining (4096).

The server **inlines** calls at submission (`graph::functions::inline`, deterministic). An inner tensor
of call `h1` is named `h1/<name>`. What then goes to the workers, the validator and the plan is a flat
description. Nothing else changes, whether stages, shards or matrix tiling. The submitted description
remains the source of the run (`source_graph`). `validate` rejects a description that has not been
inlined, with an explanation. Without functions, the field is not serialized: existing descriptions keep
their bytes.

### 4. On the PyTorch side: `gpugoblin.formula`, and a richer driver

`gpugoblin.formula(expr, *tensors)` is the custom operator for the PyTorch developer.

- **In PyTorch**, it is a registered operator (`torch.library.custom_op`). It computes the expression
  with torch operations and is differentiated by recomputation.
- **At capture**, it becomes **one** `Formula` node.
- **Before anything is sent**, the grammar is checked in Python with the same messages as in Rust.

In addition, the driver translates the following with existing operators:

- `chunk`, `split`, `unbind`, `select`, `squeeze` and `stack`;
- `conv1d`, seen as a `Conv2d` of height 1;
- `group_norm`, written with `ReduceMean` and `Formula`;
- `adaptive_avg_pool2d` and a mean over several axes;
- `expand` over the batch;
- empty constant attention masks and computed additive masks;
- an embedding row shared by the whole batch;
- in-place variants (`add_`);
- **`nn.LSTM` unrolled over time**: stacked layers, unidirectional, batch first;
- about twenty element-wise functions (`exp`, `softplus`, `clamp`, `leaky_relu`, `elu`, `mish`,
  `hardswish`, …) mapped to `Formula`.

## What the measurements establish

**Capturing proves nothing.** `tests/test_corpus.py` therefore runs the full path for every captured
model:

1. perturbed weights, so that a zero bias or a unit gain cannot hide an error;
2. capture;
3. validation by the Rust tool;
4. execution by the Rust tool;
5. comparison of the output with PyTorch's.

| Property | Evidence |
|---|---|
| Coverage | **16 models out of 18**, including Mamba and the LSTM, which compute **the same function** as PyTorch: relative difference ≤ 4.8e-7 for 15 of them, 7.5e-6 for Gemma (tanh-approximated GELU, embedding multiplied by √d); test tolerance: 1e-5 (`tests/test_corpus.py`) |
| Bugs found by this check | three **long-standing** driver bugs, invisible as long as outputs were not compared: the shape of a broadcast `matmul` (`[64,2]×[N,2,8]`), the shape of a product broadcast in both directions, `flatten(2)` translated as `flatten(1)`. A safeguard now checks every shape computed by the driver against the one measured by PyTorch |
| `Formula`, value and derivative | `crates/gg-compute/tests/graph_formula.rs`: 12 expressions covering the whole grammar, compared with their hand-written f64 definition (value ≤ 2e-6 relative, gradient ≤ 1e-5), bit-for-bit determinism, SiLU as a formula = `Mul(x, Sigmoid(x))` |
| `Formula` on the GPU | `tests/gpu.rs::formula_on_gpu`: an expression using every function, loss identical to the CPU, gradient within 1.9e-6 (validator tolerance: 1e-4) |
| Python and Rust agree | `gg-python/tests/test_formula.py`: same rejections, with the same messages, for 12 cases; 8 expressions evaluated by torch and by the Rust executor, difference ≤ 2e-6 |
| Inlined functions | `tests/graph_functions.rs`: three residual blocks with a nested function, **identical byte for byte**, value and gradient, to the same description written flat; named rejections; bounded inlining |
| On the server | `tests/collective.rs::a_description_with_functions_is_inlined_and_trains`: description submitted with functions and `Formula`, inlined by the server, trained by two verified workers; checkpoint = single-process replay (≤ 1e-6) |
| On the real grid | see docs/STATUS.md (P11, operators) |

## What remains outside the profile

- **T5.** Its relative position bias is computed from positions through integer operations (`log`,
  rounding, `where`): this would require integer tensors computed inside the graph.
- **GNN graphs** (`index_add`, indexing by a tensor). The execution model splits the batch by rows; a
  graph whose nodes are the batch does not fit into it. A formulation with fixed-size graphs, for
  example, will be needed.
- **A bidirectional LSTM**, or one with a provided initial state or sequence-first layout.
- **BatchNorm in inference mode**: the platform trains, and BatchNorm normalizes over the batch there
  (documented in [gg-python/docs/PYTHON.md](https://github.com/GPU-Goblin/gg-python/blob/main/docs/PYTHON.md)). The corpus check therefore compares against PyTorch in that mode.
- **A hand-written derivative** (`torch.autograd.Function` with a custom `backward`): only the forward
  pass is captured, and the derivative is the one from automatic differentiation.
