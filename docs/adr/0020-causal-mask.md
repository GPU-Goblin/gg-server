# ADR-0020 — Profile `gg-ops/2`: the causal mask, and causal attention captured from PyTorch

Status: accepted (2026-09-21). Complements ADR-0016.

## Context

A language model learns to predict the next token. If it sees the whole sequence, it learns nothing:
it **reads the answer**. The `gg-ops/1` profile had no causal mask, so a transformer described on the
grid was sized correctly but did not train correctly. It was the most visible
limitation for a researcher.

Two more obstacles prevented writing causal attention in ordinary PyTorch: the driver rejected every
common way of building the mask, and all of them come with a scaling by a scalar, 1/√d, which the
format could not express.

## Decision

### 1. A `CausalMask` operator, and the profile moves to `gg-ops/2`

`CausalMask` takes attention scores `[batch, …, T, T]` and replaces with a mask value those at
positions `(i, j)` where `j > i`, i.e. where the key comes **after** the query. It goes before the
`Softmax`.

- **Shapes**: rank 3 or more, last two axes square and known. A rank-2 input would put the batch axis
  inside the score matrix: rejected, with an explanation. Different query and key lengths are rejected
  too; they only occur in incremental inference, outside this computation type.
- **Mask value: −10⁹, not −∞.** After the `Softmax`, the weight is exactly zero in both cases, since
  `exp(−10⁹ − m)` underflows to zero in f32. But a finite value never produces a `NaN`, even if a whole
  row were masked or if the mask were followed by something other than a `Softmax`.
- **The mask is built on the host**, true at `(i, j)` when `j > i`. Burn's `triu_mask` function uses
  the opposite of the intuitive convention; building the mask ourselves makes the semantics independent
  of the engine.
- **Cost**: no parameters, one comparison per score element.
- `gg-ops/2` is a superset of `gg-ops/1`: every description valid under one is valid under the other.

In line with ADR-0016, the operator comes with everything an operator must bring:

| Requirement | Where |
|---|---|
| Implementation | `crates/gg-compute/src/graph/exec.rs`, CPU and GPU |
| Verification oracle | test against a naive definition, byte for byte, ranks 3 and 4 |
| Determinism | `Bitwise`: a selection, no arithmetic |
| Measured tolerance per profile | GPU/CPU difference 3.7e-9 on RTX 5060 Ti, in the per-operator table |
| FLOP estimator | one per score element |
| Memory estimator | same shape as the input, no parameters |

### 2. A constant initialization for model scalars

`{"constant": {"value": 0.125}}` joins `zeros`, `ones`, `xavier_uniform` and `normal`. A model scalar —
the 1/√d factor, a −1 — is declared as a **frozen parameter** with a constant initialization. The
description stays self-contained: it computes the right function without depending on a checkpoint. A
non-finite value is rejected, like a negative standard deviation.

### 3. The driver recognizes a causal mask **by its value**, not by the code that produces it

The driver computes the concrete value of every node of the captured graph. Any subgraph that depends
on neither an input nor a parameter — a buffer, `torch.ones`, `torch.triu`, a comparison — is a
constant: it is not emitted. A `masked_fill` or a `where` that applies such a constant with a "−∞"
value becomes `CausalMask` **if and only if** the constant is exactly the strict triangle of future
positions. Any other constant, a data-dependent mask, or a fill value other than "−∞" is rejected,
with the reason named.

Recognizing by value covers every way of writing it at once, including the ones nobody anticipated,
and cannot be fooled by a mask that looks like the triangle without being it.

`scaled_dot_product_attention` is decomposed into profile operators: transpose of `k`, product,
scaling by a frozen constant, `CausalMask` if `is_causal`, `Softmax`, product. The following are
rejected by name: an explicit `attn_mask`, dropout inside attention, grouped-query attention.

## What the measurements establish

| Property | Evidence |
|---|---|
| **No information flows back from the future** | changing token `j` modifies none of the positions `< j`, **byte for byte**, in direct, segmented and GPU execution; the same test without a mask sees the leak, which proves it can detect one |
| **The mask prevents cheating** | on randomly drawn, hence unpredictable, tokens, the best possible loss is ln V = 3.466. Without a mask, the model drops to **2.734**: it reads the future. With the mask, it stays at **3.478** |
| **The capture computes the same function as PyTorch** | four ways of writing causal attention — a buffer and `masked_fill` in the nanoGPT style, `torch.triu` in `forward`, `scaled_dot_product_attention(is_causal=True)`, `torch.where` — : Rust executor output within ≤ 1e-5 of PyTorch's (`gg job eval-model`) |
| **A language model trains on the grid** | module written like nanoGPT, real server, worker and validator, next-token prediction: **loss 2.99 → 0.0000, accuracy 100 %** in 40 rounds |
| GPU | logits of a causal transformer within 8.6e-7 of the CPU reference |

## Two earlier bugs uncovered along the way

- **A captured tensor constant was taken for a model input**, which shifted all subsequent inputs. An
  overly broad condition in the driver was the cause.
- **A module with a buffer could not reload its trained weights**: reloading required the checkpoint to
  cover the buffers too, and those are not trained. Only parameters are now required; buffers keep
  their value.

The second affected every module with a buffer, whether a mask or batch-normalization statistics.

## What is not done

- **Positional encoding.** The causal mask alone breaks the symmetry between positions, and a causal
  transformer without an encoding can infer order from it. This is not measured here: the demo task
  depends only on the current token. A learned or sinusoidal encoding comes next.
- **Non-causal masks**: sliding windows, padding masks, arbitrary `attn_mask`.
- **Incremental inference** with a key-value cache, where queries and keys differ in length.

## Consequences

- **Compatibility**: a worker older than `gg-ops/2` rejects a description that uses `CausalMask`,
  naming the operator, when validating the task. Nothing runs wrongly, but a fleet with mixed versions
  would see these tasks fail and then be reassigned. The task does not yet carry the profile version it
  requires; this must be added before any heterogeneous deployment.
- The `gg job template transformer` skeleton is causal by default; `--bidirectional` makes it
  bidirectional, like an encoder.
- `gg job eval-model` evaluates a description offline. It is used to test that a capture computes the
  same function as its module, which the driver tests did not check until now.
