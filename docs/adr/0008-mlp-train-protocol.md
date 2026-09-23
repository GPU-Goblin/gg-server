# ADR-0008 — The `mlp.train.v1` training protocol

Status: accepted (2026-09-16)

## Context

The first real training type must be reproducible by an independent validator without depending on a
backend's seed or on an implicit data order.

## Decision

- **Data**: four f32 GGT1 artifacts, `train_x [n,d_in]`, `train_y [n,d_out]`, `eval_x`, `eval_y`.
  The demonstration dataset is synthetic (`gg data gen-mlp`): a "teacher" tanh network with one hidden
  layer drawn from the DRBG (`mlp.data.v1/<seed>`), inputs uniform in [-1, 1), approximately Gaussian
  noise (sum of 12 uniforms) of fixed amplitude; train and eval are materialized separately.
- **Model**: `relu(x·W1 + b1)·W2 + b2`, W1 `[d_in,d_h]`, W2 `[d_h,d_out]`, biases initialized to zero,
  W1 and W2 uniform in `±1/sqrt(fan_in)`, drawn in that order from the `mlp.train.v1/init` DRBG with the
  task's seed. No backend seed is involved.
- **Example order**: the index stream is the concatenation of Fisher–Yates permutations
  `mlp.train.v1/order` (task seed), one per epoch; step `s` consumes indices
  `[s·batch, (s+1)·batch)` of the stream. Batches never partially straddle two epochs: they are
  consecutive slices of the stream (full batches).
- **Loss / optimizer**: MSE averaged over the batch; `sgd` (no momentum, no decay) or `adam`
  (β1 0.9, β2 0.999, ε 1e-5 — the Burn 0.21 defaults, frozen under the profile name
  `adam-burn-0.21-v1`). Learning rate: a finite f64 in (0, 1].
- **Outputs**: `w1.ggt`, `b1.ggt`, `w2.ggt`, `b2.ggt` and `metrics.json` (≤ 64 KiB) containing
  `steps_completed`, `resumed_from_step`, a bounded log `[{step, train_loss}]` (≤ 64 entries) and a
  recomputable `eval_loss`.
- **Verification**: the validator replays the complete training on the CPU reference profile and
  compares the four weight tensors (within the policy's tolerance), then recomputes `eval_loss` from the
  submitted weights and compares it with the declared value (relative tolerance 1e-3). Declared metrics
  are never evidence.
- **Checkpoints**: the executor writes `outputs/checkpoint/` (weights, current step, optimizer state in
  Burn's binary format) at a bounded interval; when restarted on the same bundle, it resumes and declares
  `resumed_from_step`. A local checkpoint is never promoted to an official result without validation.

## Consequences

The CPU and GPU numeric profiles drift apart slightly after several hundred steps; tolerances are
measured (docs/BENCHMARKS.md) before the GPU profile is admitted for this type.
