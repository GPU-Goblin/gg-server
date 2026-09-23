# ADR-0022 — Profile `gg-ops/4`: scaled RoPE, described by its frequencies

Status: accepted (2026-09-21). Complements ADR-0021.

## Context

ADR-0021 brought RoPE with its original law, `ω_i = 1 / base^(2i/d)`. But the large open models a
researcher will want to adapt — the platform's realistic use case for very large models — **extend
their context by changing these frequencies**:

| Variant (`rope_type`) | Models | What changes |
|---|---|---|
| `llama3` | Llama 3.1, 3.2, 3.3 | high frequencies untouched, low ones divided by 8 or 32, smoothed transition |
| `yarn` | long-context Qwen 2.5, DeepSeek V2 and V3 | a ramp between extrapolated and interpolated frequencies, and an **amplitude** that multiplies cosine and sine |
| `longrope` | Phi-3, Phi-3.5, Phi-4-mini | one factor per frequency, short or long depending on the sequence, and an amplitude |
| `linear` | extended-context fine-tunes | every frequency divided by a factor |
| `dynamic` | dynamic NTK | the base grows when the sequence exceeds the context |

The chain was broken in three places: the description could not express them, the PyTorch driver
rejected their tables, and the skeleton could not generate them. A Llama 3.1 therefore did not go
through, even though all of its operators are in the profile.

## Decision

### A rotary table is described by its frequencies, not by its variant

Scaling is just a way of computing the frequencies `ω_i`, plus, for YaRN and LongRoPE, an amplitude
`A`. The table generator therefore gains a second form, mutually exclusive with the first:

```json
{"rotary_cos": {"base": 500000}}
{"rotary_cos": {"frequencies": [1.0, 0.8058, …, 3.77e-7], "amplitude": 1.1386}}
```

The executor has **one** formula, `A·cos(t·ω)` and `A·sin(t·ω)`, computed in f32 in the reference's
order. It knows no variant by name. Three reasons:

- **the verification surface stays a single formula**, instead of one per variant to be kept identical
  across the worker, the validator and the driver;
- **a future variant requires nothing**: the next published scaling will be another list of
  frequencies;
- **this is what the driver can recover from the value**, without guessing the variant. Different
  Llama 3.1 parameter sets give the same frequencies (original context and thresholds multiplied by
  the same number): the variant cannot be identified from the table, the frequencies can.

Validation, with named rejections: a base **or** frequencies, not both; as many frequencies as pairs
of dimensions; each frequency finite and non-negative ("proportional" RoPE leaves some pairs still);
amplitude finite and strictly positive, and only with explicit frequencies.

### An unknown field is rejected, everywhere in the description

The explicit form revealed a broader flaw. A worker from an earlier version reading
`{"base": 10000, "amplitude": 1.2}` ignored the field it did not know and **silently computed a
different function from the one described**. If the validator is old too, both agree on the wrong
result.

Every structure in the description now rejects an unknown field: root, inputs, parameters,
initializations, nodes, loss. This is a behavior change: a description that carried a superfluous
field, ignored until now, is now rejected, with the field named.

`gg-ops/4` adds no operator. It remains a superset of `gg-ops/3` for any description without
superfluous fields.

### The driver recovers the frequencies from the pair of tables

The cosine alone is not enough. At Llama 3.1's low frequencies (`ω ≈ 4e-7`), `cos(t·ω)` is exactly 1
in f32 over a whole short sequence: the table says nothing about ω. The pair, however, gives the angle
`atan2(sin, cos)` at each position. Unwrapped along t, its slope is ω. Each frequency is then adjusted
among its neighbors within ±2 ulp, to reproduce both tables as closely as possible.

The driver looks for the partner among constant tensors of the same shape, and accepts the pair if
both regenerated tables are within the recognition tolerance, `max(2e-5, 5e-7·T)·A`. The original law
is preferred when it fits, because it is compact and readable. Both tables get **the same law**: their
descriptions cannot diverge.

A negated cosine, the sine of a different law, a one-position shift or a different amplitude between
the two tables are rejected. A table on its own is only recognized under the original law, as before.

### Variants that change frequencies on the fly

`transformers` decides, **inside the forward pass**, whether dynamic NTK or LongRoPE should change
frequencies, by comparing the length with the original context. `torch.export` rejects such a branch
on a computed value, before the platform even sees the model.

For a fixed length, however, the choice is known. `freeze_rotary_embeddings(module, T)` runs each
rotary module once at length T, in plain Python, which installs the frequencies for that length. It
then replaces its forward pass with the same one, minus the update. The model's output at that length
does not change, and this is verified. The driver suggests this call when `torch.export` hits this
case.

### The skeleton reads a `config.json`

`gg job template transformer --rope-scaling` takes as is the `rope_scaling` object (transformers 4) or
`rope_parameters` object (transformers 5) of a `config.json`, inline or as `@file.json`.
`--rope-max-positions` gives the `max_position_embeddings` consulted by dynamic NTK, YaRN and
LongRoPE. The formulas reproduce those of `transformers` **in their f32 order of operations**: `torch`
converts each Python scalar to f32 before the operation, and `s / x` there equals
`reciprocal(x) · s`. The skeleton writes them in explicit form.

The skeleton rejects, with an explanation, an unknown key (`mrope_section`, which changes the table
layout), an unknown variant, and partial rotation: it rotates the whole head, whereas the PyTorch
driver captures a model that rotates only part of it.

## What the measurements establish

| Property | Evidence |
|---|---|
| Validation | explicit form accepted, ten named rejections; an unknown field rejected at seven places in the description |
| The explicit table follows its formula | difference ≤ 3e-5 from an f64 formula over 256 positions, amplitude 1.35 |
| Explicit form and original law | same tables byte for byte when the frequencies are those of the law |
| **Linear interpolation** | the table with factor `s` at position `s·u` equals the original table at `u`: **identical byte for byte** for `s` a power of two, to 1e-5 for `s = 3` |
| Llama 3.1 | high frequencies identical byte for byte, low ones divided exactly by 8, intermediate ones bracketed, order preserved |
| YaRN | amplitude `0.1·ln(factor) + 1`; highest frequency untouched; lowest interpolated to within 1e-6 |
| LongRoPE | short factors up to and including the original context, long ones beyond; amplitude `sqrt(1 + ln f / ln L)` |
| **All keep the RoPE property** | dot product a function of the offset alone, to 2e-5 for YaRN and Llama 3.1; squared norm multiplied by A², to 1e-5 |
| **Rust skeleton vs `transformers`** | 16 configurations: **14 identical byte for byte**, the other two within 1 and 2 ulp (YaRN without truncation, NTK beyond the context: SLEEF's f32 power and libm's differ by one ulp); amplitudes identical |
| **Frequencies recovered by the driver** | from the table values alone, all 16 configurations: **exact** f32 frequencies over 512 positions, regenerated tables identical byte for byte from 16 positions on |
| **The real `LlamaRotaryEmbedding` class** captured | original law, linear, Llama 3.1, YaRN, dynamic NTK and LongRoPE (frozen), YaRN with partial rotation: Rust executor within ≤ 3.6e-7 of PyTorch, with the module's tables as well as with the generator's |
| Rust generator vs `transformers`, 2,048 positions | exact frequencies: 96.9 % of values identical byte for byte, difference 1.2e-7 (the cosines of two libraries) |
| GPU | YaRN transformer, gradients within 5.6e-5 of the CPU, no validator rejection |
| **On the real grid** | the real `transformers` rotary class with YaRN (amplitude 1.139), copy at t−2: **accuracy from 6.0 % to 93.3 %**, loss 2.99 → 0.27, in 60 rounds, with server, worker and validator |

## A claim from ADR-0021 corrected

ADR-0021 stated "RoPE tables vs the Hugging Face computation: identical byte for byte". No test backed
it, and it is false in general. Measured again over 2,048 positions, about 95 % of values are identical
byte for byte, and the difference stays at one unit in the last place (6e-8) for bases 10,000, 500,000
and 1,000,000. But for some bases (150,000), Rust's f32 power and `torch`'s differ by one ulp on one
frequency, and the difference grows with position up to 1.5e-5. This is within the recognition
tolerance, and has no effect on a captured model, whose checkpoint carries the module's exact values.
The measurement is now a test (`test_rust_tables_match_the_reference`).

## What is not done

- **Interleaved RoPE** (GPT-J, CodeGen, Meta's complex layout): pairs `(2i, 2i+1)` taken as stride-two
  slices, then stacked. A `Slice` stride and a transient rank 5 are missing.
- **Multimodal RoPE** (`mrope_section`, Qwen2-VL): rejected by the skeleton; not tried with the driver.
- **Multiple layer types** (Gemma 3, local and global frequencies): `freeze_rotary_embeddings` rejects
  these modules; capturing them without freezing has not been tried.
- **"Proportional" RoPE** (zero frequencies): the description accepts it, but no real model has been
  captured with it.
- **ALiBi** and T5's **relative position biases**.
- **The required profile version is still not carried by the task.** Rejecting unknown fields closes
  the most dangerous case, an old worker silently computing something else. It remains that a
  `gg-ops/4` task handed to a `gg-ops/3` worker fails and is then reassigned, instead of never being
  offered to it.

## Consequences

- `gg job template transformer --rope-scaling '<json>' [--rope-max-positions N]`.
- `gpugoblin.freeze_rotary_embeddings`, and a driver message that points to it.
- `graph::rope` exposes the `transformers` formulas, for anyone building a graph programmatically.
- `graph::Rotary` replaces the single `base` field of rotary tables. `TransformerDims` is no longer
  `Copy`, because LongRoPE carries lists.
