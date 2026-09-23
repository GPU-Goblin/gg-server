# ADR-0023 — Profile `gg-ops/5`: `transformers` models captured as they are

Status: accepted (2026-09-23). Step C3 of the P11 plan (docs/ROADMAP.md). Complements ADR-0016, and
ADR-0020 to ADR-0022.

## Context

The 2026-09-23 audit tried what a researcher would try first: hand a `LlamaForCausalLM` or a
`Qwen2ForCausalLM` from `transformers` to the driver. **Both were rejected.** An inventory of the
exported graph listed the gaps:

| What `transformers` writes | What was missing |
|---|---|
| hand-written RMSNorm: `x.pow(2).mean(-1, keepdim=True)`, `torch.rsqrt(var + ε)` | `pow`, `mean`, `rsqrt` |
| SwiGLU network: `down(silu(gate(x)) · up(x))` | `silu` |
| grouped-query attention (GQA): `repeat_kv`, i.e. `x[:, :, None].expand(…).reshape(…)` | `unsqueeze` to rank 5, `expand` |
| `_assert_tensor_metadata`, RoPE tables computed under `no_grad` | nodes that compute nothing |
| `q.view(b, t, -1, d)` | a driver bug: a `-1` outside the first position was taken for the batch |
| a frozen base (`requires_grad=False`) | the driver trained it anyway |

## Decision

**Two generic operators enter the profile; everything else is composed.**

1. **`ReduceMean`**: mean along one axis, kept with size 1 (`keepdims = 1`, the default) or removed.
   Rejects the batch axis, which would mix examples, and an axis out of rank. Tolerance-checked.
2. **`Rsqrt`**: element-wise `1/√x`. Tolerance-checked.

They are deliberately generic: Llama's RMSNorm, Gemma's (`1 + w`), T5's LayerNorm and a mean over the
sequence are all written with them, whereas recognizing each way of writing it and mapping it to
`RMSNorm` would break with the next variant.

The driver translates the rest with existing operators:

- `silu(x)` → `x · Sigmoid(x)`;
- `pow(x, 2)` → `x · x`; any other exponent is rejected with an explanation;
- `unsqueeze` → `Reshape`;
- **repeating key heads** (GQA) without a rank-5 tensor: `n` copies laid end to end along the sequence
  axis (`Concat`, in groups of eight), then a `Reshape` that folds that axis into the head axis. Key
  head `j` thus serves queries `j·n … j·n + n − 1`, exactly as `repeat_kv` does.
  `scaled_dot_product_attention(…, enable_gqa=True)` takes the same path;
- a constant `attn_mask` that **equals** the causal mask (boolean or additive), recognized by its value
  as in ADR-0020;
- `torch.export`'s checking nodes are ignored;
- a `requires_grad=False` parameter becomes `"trainable": false`.

## What the measurements establish

| Property | Evidence |
|---|---|
| `ReduceMean` follows its definition | naive f64 oracle on a rank-4 tensor, three axes, `keepdims` 0 and 1, negative axis: difference ≤ 1e-6 |
| `Rsqrt` follows its definition | `rsqrt(x)·√x − 1` ≤ 3e-7 over 256 values from 1e-3 to 100 |
| Named rejections | mean over the batch, axis out of rank, `keepdims = 2`, missing axis |
| **Hand-written RMSNorm equals `RMSNorm`** | identical loss to 1e-6; gradients within **2.4e-7**; two runs identical byte for byte |
| GPU | `ReduceMean` at 1.5e-8, `Rsqrt` at 4.5e-8; **a Llama captured from `transformers`** (GQA, Llama 3.1 RoPE, 136 nodes): identical loss, gradients within **7.5e-8** of the CPU, no validator rejection |
| **Real `transformers` models** | Llama with 4, 2 and 1 key heads, Qwen2, Mistral, scaled Llama 3.1, two layers each: the Rust executor computes the same function as PyTorch, difference **≤ 1.5e-7** (`gg-python/tests/test_hf_models.py`) |
| Frozen base | only `lm_head` trained: the description says so, and the trained parameter count follows |
| On the real grid | see docs/STATUS.md, section P11 C3 |

## What is not done

- **No sliding window** (Mistral with `sliding_window`, Gemma 2): the mask is no longer the causal
  triangle, and it is rejected with an explanation.
- **No padding mask**: sequences of different lengths in the same batch.
- **No bf16**: the model is captured and executed in f32; a bf16 checkpoint is converted by PyTorch
  before capture.
- **Size**: the models tried have two layers and a width of 32. A production model then runs into a
  contributor's memory limit, which D2 (staged execution) must lift.
- **Rotary modules with several layer types** (Gemma 3) and **multimodal RoPE** (Qwen2-VL) are not
  translated.

## Consequences

- `gg-ops/5`: 26 operators. A task that uses `ReduceMean` or `Rsqrt` requires `gg-ops/5`, and the
  server only offers it to workers that announce it (C1).
- `crates/gg-compute/tests/fixtures/llama-hf-mini.json`: a genuinely captured description, used as a
  Rust-side reference without depending on Python.
