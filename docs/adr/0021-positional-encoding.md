# ADR-0021 — Profile `gg-ops/3`: positional encoding, without a specialized operator

Status: accepted (2026-09-21). Complements ADR-0016 and ADR-0020.

## Context

The causal mask (ADR-0020) prevents a model from reading the future, but does not tell it **where**
each token is. Yet many tasks depend on order: copying, shifting, counting, following a syntax. ADR-0020
left a question open: measure what a causal transformer learns about order without an encoding.

Three encodings cover what a researcher runs into:

- **learned positions** (GPT-2, nanoGPT): one trained vector per position, added to the embedding;
- the fixed **sinusoidal table** from the original paper;
- **RoPE rotations** (Llama, Mistral, Qwen): queries and keys are rotated by an angle proportional to
  the position. This is the encoding of the large open models, hence the one required to adapt an
  existing model, which has been identified as the platform's realistic use case for very large models.

## Decision

**No specialized operator.** The profile gains a single generic operator, plus table generators. All
three encodings are composed from operators that are already verified.

1. **`Slice`**: a contiguous slice `[start, end)` along one axis. Rejects the batch axis, and an empty
   or out-of-range slice. Bitwise, no computation. It is used for RoPE's half rotation and for learned
   positions when the table is longer than the sequence.
2. **Three initialization generators**, for frozen parameters of shape `[1, …, 1, T, d]`: `sinusoidal`,
   `rotary_cos`, `rotary_sin`, parameterized by their base. They compute in f32 like the reference
   models: `ω_i = 1 / base^(2i/d)`, angle `t·ω` in f32, then sine or cosine. The description stays
   self-contained and compact: `{"rotary_cos": {"base": 10000}}` instead of thousands of values.
3. **RoPE is a composition**: `x·cos + [−x₂, x₁]·sin`, i.e. two `Slice`, one `Mul` by the constant −1,
   one `Concat`, two `Mul` by the tables and one `Add`. The layout is the so-called `rotate_half` layout
   of the Llama models in Hugging Face. Seven nodes per rotated tensor.
4. **Learned positions** are a trained `[T, d]` parameter added by broadcasting, preceded by a `Slice`
   if the table has more rows than the sequence.

`gg-ops/3` is a superset of `gg-ops/2`.

### The PyTorch driver recognizes tables by their value

As with the causal mask, the driver does not guess from how the code is written. Every non-uniform
constant tensor consumed by the computation is compared with the tables the platform can generate: the
base is estimated from row `t = 1`, where the angle equals the frequency, then the whole table is
regenerated and compared, with a tolerance that grows with position since the angle is computed in
f32. Common round bases are tried as well, because estimating a base from a cosine is ill-conditioned
for small head widths.

A recognized table becomes a frozen parameter whose **description names the generator** and whose
**checkpoint carries the module's exact values**. An unrecognized table — noise, another formula, a
scaling variant — is rejected with an explanation. `wpe(arange(t))` becomes the parameter itself or a
slice of it.

## What the measurements establish

**The open question from ADR-0020.** Copy the token two positions earlier, on randomly drawn tokens;
no-information floor ln V = 2.773, one layer, 300 steps:

| Encoding | Final loss |
|---|---|
| none, causal mask only | 1.360 |
| learned positions | **0.001** |
| RoPE | **0.253** |

**The mask alone provides part of the order, but does not solve the task.** This is consistent with
the idea that causal attention can infer a position from the number of tokens it sees; it is not
consistent with the idea that this is enough.

| Property | Evidence |
|---|---|
| `Slice` follows its definition | naive oracle, byte for byte, on five slices of a rank-4 tensor; named rejections |
| The tables match their formula | difference ≤ 1e-5 from an independent f64 formula |
| **RoPE only sees the offset between positions** | after rotation, the dot product of positions `(m, n)` equals that of `(m+s, n+s)` to 1e-5, for four pairs; this is the property that defines RoPE, and the oracle for the composition |
| Rust generator vs reference | **6e-8**, one unit in the last place, up to 2,048 positions over 128 dimensions |
| RoPE tables vs the Hugging Face computation | ~~identical byte for byte~~ — **corrected by ADR-0022**: about 95 % of values identical byte for byte over 2,048 positions, a one-ulp difference (6e-8) for common bases, up to 1.5e-5 when the f32 power diverges by one ulp (base 150,000) |
| PyTorch captures | learned positions, sliced learned positions, sinusoidal, RoPE with base 10,000 and 500,000: Rust executor output within ≤ 5e-7 of PyTorch |
| GPU | `Slice` at 3.7e-9; RoPE transformer, gradients within 5.5e-5 of the CPU, no validator rejection; learned positions at 3.5e-5 |
| **On the real grid** | RoPE model written in the Hugging Face style, copy at t−2: **accuracy from 6 % to 92 %**, loss 2.99 → 0.25, in 60 rounds, with server, worker and validator. Two runs give the same figures byte for byte |

## An earlier bug fixed

`torch.cat(..., dim=-1)` produced a `Concat` with a negative axis, which validation rejects. RoPE's
half rotation uses it; the driver now normalizes the axis.

## What is not done

- **RoPE scaling variants** — Llama 3.1's `rope_scaling`, YaRN, NTK — change the frequencies: their
  tables are not recognized and the driver rejects them with an explanation. *Done since: ADR-0022,
  profile `gg-ops/4`.*
- **Interleaved RoPE** (GPT-J layout, pairs `(2i, 2i+1)`), **ALiBi** and T5's **relative position
  biases** are not translated.
- **The required profile version is still not carried by the task.** Every version bump widens the
  gap: a worker older than `gg-ops/3` rejects a description that uses `Slice` or a generated table,
  naming it, and the task is then reassigned. Fixing this requires the worker to announce its profile
  when claiming, the task to carry the minimum required profile, and the server to filter: protocol,
  database and claim path. This must be done before any heterogeneous deployment.

## Consequences

- The `gg job template transformer` skeleton takes `--positions rotary|learned|none`, RoPE by default,
  and `--rope-base`.
- `templates::rotary` composes the rotation for anyone building a graph programmatically.
- The parameter count now distinguishes **trained** from **frozen** parameters: RoPE tables are
  parameters in the format's sense, not in the learning sense.
