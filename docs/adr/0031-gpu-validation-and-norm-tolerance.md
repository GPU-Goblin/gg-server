# ADR-0031 — Validation on the GPU, engine checks on the CPU, and a tolerance by norm for model tasks

Status: accepted (2026-09-24).

## Context

Measured on one RTX 5060 Ti (docs/STATUS.md, "The GPU grid against a single GPU"), a grid round is
decided by the validator's CPU recomputation:

- ViT-tiny, 32 images: 7.2 s on the GPU worker, then 15 s on the CPU validator.
- ConvNeXt-tiny: 12.7 s on the worker, then 42.7 s on the validator.

A GPU worker computes faster than a CPU validator can check it, so adding GPU workers does not make
rounds shorter.

The same measurement showed that the `graph.grad.v1` tolerance was wrong for real models. The rule
was element by element, `|a − b| ≤ 1e-4 + 1e-3·|b|`, calibrated on two-layer transformers. On a
ResNet18 at 224 px and 32 images, a GPU result as accurate as PyTorch's was rejected. Measured against
float64:

| Engine | Median relative error | Largest element gap |
|---|---|---|
| CPU reference | 2.7e-3 | 7.5e-4 |
| GPU | 3.9e-3 | 1.0e-3 |
| PyTorch FP32 on CUDA | 5.3e-3 | 1.1e-3 |

The rule was tighter than the rounding of its own reference, once a batch normalization sums over
32 × 112 × 112 values. It is not a matter of one result: any normalized convolutional network at a
realistic batch is rejected by it.

## Decision

### 1. A tolerance by norm for model tasks

`graph.grad.v1` and `graph.stage.v1` are compared per tensor by the norm of the difference:

```
‖a − b‖ ≤ rel·‖b‖ + abs·√n        rel = 2e-2, abs = 1e-7
```

This is `recompute-reference` **v2**, and the catalog becomes version 2. Other kinds keep v1 and
their element-wise tolerances.

- **`rel`.** It is four times the widest honest gap measured on the pilot models. Per tensor, GPU
  against the CPU reference reaches 5.3e-3 at most (ResNet18); the CPU against float64 reaches
  4.3e-3, and PyTorch FP32 7.1e-3.
- **`abs`.** It is a root-mean-square per element, for gradients that are zero in exact arithmetic,
  where both sides are pure rounding noise. Examples are a key bias before a softmax, or a bias
  before a normalization.
  - It sits at the level of float32 rounding. An `abs` of 1e-4 would let a gradient of that order be
    entirely wrong and still pass.
  - Measured: wav2vec2's key bias passes at 1e-7.
- **Caps.** `rel` is capped at `MAX_NORM_TOLERANCE` = 2e-2, the measured value and no more. The
  element-wise cap `MAX_TOLERANCE` (1e-2) is unchanged.
- **Guards.**
  - A computation that differs is refused, whether the deviation is spread or concentrated on a
    single element (`compare::tests`).
  - The Burn convolution defect (100 % error) is refused by far.
  - `tests/gpu.rs` asserts that every honest GPU gradient of 12 described models passes this rule.

### 2. Validators may recompute model tasks on the GPU

A validator configured with `engine = "gpu"` (built with the `gpu` feature) recomputes model tasks
on its GPU. Every other kind is still recomputed on the CPU.

- **Process.** The validator keeps its kernels compiled from one verification to the next; Vulkan is
  initialized once per process.
- **Reference.** The task's `reference_profile` stays the CPU profile: it names the reference
  function.
- **Decision.** A decision states the profile it was actually recomputed with. The server refuses a
  profile that is not admitted as a reference for the kind (`profiles::reference_admitted_for`): the
  CPU profile everywhere, the GPU profile for model tasks only.

### 3. Engine checks: a CPU sample of what a GPU validator decided

A GPU reference shares the GPU workers' software. An engine defect common to both passes any
GPU-against-GPU comparison. The Burn convolution defect found on 2026-09-24 would have: the input
gradient of a convolution whose kernel is shorter than its stride was wrong on the GPU, and a GPU
validator would have made the same error. Hence the engine check.

- **When.** When a decision recomputed on the GPU is recorded, the server draws with
  `engine_check_rate` (default 0.1). The draw uses the server-side secret under its own domain, so
  neither the worker nor the validator can predict or influence it. If selected, an `engine_check`
  verification is opened in the same transaction.
- **Who.** It is offered only to validators recomputing on the CPU, and its decision must state the
  CPU profile. It checks the engine, not the operator: no operator rule applies to it. Its CPU
  recomputation differs by construction from the GPU decision it checks, and the CPU validator that
  audited the same submission may take it; the redundant recomputation this sometimes costs is
  about 2.5 % of tasks at the default rates (0.25 × 0.1).
- **Dedicated validators.** A validator with `secondary_only = true` takes only secondary
  verifications, audits and engine checks, never a primary one. A CPU validator configured so sits
  next to GPU validators, which take the primary verifications. It must take audits too: an audit
  by another operator cannot go to the GPU validator that decided the primary.
- **Effect.** It counts like an audit (ADR-0014). Nothing is aggregated or paid before it decides,
  and a disagreement opens a dispute settled by a human, never a sanction.
- **Why a sample is enough.** An engine defect is systematic: every task of a given shape shows it.
  At 10 %, the probability of missing it on 50 affected tasks is 0.9⁵⁰ ≈ 0.5 %.

## Consequences

- **Speed.** Rounds are no longer bounded by the CPU recomputation, except for the sampled ones.
- **What a dishonest worker can do.** Within the tolerance, it can bias each gradient it returns by
  up to 2 % of that gradient's norm, per tensor and per task, and still be accepted.
  - Before, the per-element bound was tighter for large values and looser for near-zero ones.
  - The bias is bounded, it is diluted by the aggregation of the round's tasks, and every task is
    still recomputed. It is written in THREAT_MODEL.md as a residual risk.
- **Protocol.** Nothing changes in the signed messages; the rule is carried by the method version
  in the validation policy. A worker or validator with catalog 1 refuses catalog-2 tasks cleanly.
- **Not done.**
  - The rate is fixed at start-up (no runtime change like the audit rate).
  - The GPU validator shares its card with nothing else by configuration.
  - Only Vulkan is measured, on one card.
