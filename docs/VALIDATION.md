# Result validation

## Principle

No acceptance relies on reported metrics. The validator (an administered identity, running the
`gg-verifier` process) replays the task from the specification and the input artifacts, then
compares the relevant outputs according to a **versioned numeric profile** and **tolerances fixed
before execution** in the `validation_policy` of the `TaskDefinitionV1`.

## Method `recompute-reference` v1

1. Check the integrity of the referenced artifacts (SHA-256, size, `GGT1` format, expected shapes).
2. Recompute the reference on the reference backend (CPU Flex, f32).
3. Compare element by element: `|a - b| <= abs + rel * |b|` (b = reference), exact shapes and
   dtypes, any non-finite value → `rejected`.
4. Produce a `VerificationDecisionV1`: checks (name, passed, measurement, tolerance), artifacts
   checked, outcome.

Outcomes: `accepted`, `rejected`, `inconclusive` (documented numerical difficulty, e.g. a profile
that cannot be verified). `inconclusive` is neither an acceptance nor an accusation of fraud.

## Admitted numeric profiles

| Profile | Backend | Status |
|---|---|---|
| `cpu-f32-flex-v1` | Burn Flex CPU f32 | admitted (P1 tests) |
| `gpu-f32-wgpu-vulkan-v1` | Burn WGPU/Vulkan f32 | admitted for `tensor.matmul.v1` (max difference 1.5e-4 at 2048³) and for `mlp.grad.v1` (bit-identical gradients on 4 configurations, a single step with no trajectory); **not admitted for `mlp.train.v1`**: out of 13 measured training runs, one diverges by 6.9e-2 on the weights (docs/BENCHMARKS.md); admission per type in `gg_compute::profiles::admitted_for`; admitted for `graph.grad.v1` and `graph.stage.v1`; **also a reference** for these two types: a validator may recompute them on the GPU (ADR-0031, below) |

A result whose profile is not admitted remains `inconclusive`.

## Tolerances per compute type

| Type | abs | rel | Rationale |
|---|---|---|---|
| `tensor.matmul.v1` (K ≤ 2048, values ∈ [-1,1]) | 1e-4 | 1e-4 | different summation orders on CPU/GPU f32; measured in `docs/BENCHMARKS.md` (GPU: max abs 1.5e-4 at K = 2048, covered by the relative term) |
| `mlp.train.v1` (final weights, recomputed evaluation loss, loss log) | 1e-4 | 1e-3 | CPU-against-CPU replay (bit-for-bit deterministic); a CPU replay of a GPU training run is not reliable (one trajectory in 13 diverges), hence `inconclusive` for the GPU profile |
| `mlp.grad.v1` (4 mean gradients of a micro-batch, recomputed `grad_meta.json`) | 1e-5 | 1e-4 | a single autodiff step; CPU vs CPU bit for bit, GPU vs CPU measured at 0 (docs/BENCHMARKS.md) |
| `graph.grad.v1`, `graph.stage.v1` (per tensor, **by norm**: `‖a − b‖ ≤ rel·‖b‖ + abs·√n`, method v2) | 1e-7 | 2e-2 | measured on the pilot models (ADR-0031): GPU against the CPU reference 5.3e-3 at most per tensor (ResNet18, 224 px, 32 images), the CPU reference itself 4.3e-3 against float64, PyTorch FP32 7.1e-3; `rel` is four times the honest gap; `abs` is a per-element root mean square for gradients that are zero in exact arithmetic. The element-wise rule used until catalog 1 (1e-4 + 1e-3·|b|) was tighter than the rounding of its own reference on normalized convolutional networks |
| Collective equivalence (`gg job collective-check`, outside validation) | 1e-4 | 1e-3 | aggregation `Σ n_i·g_i/Σ n_i` of shard gradients vs full-batch gradient: only the summation orders differ (6e-8 measured over 10 Adam rounds) |

**Gradients sent in bf16** (ADR-0035, the default for graph runs). The validator recomputes in f32
and compares what it received, rounded to bf16, under the same rule: the rounding (at most 2⁻⁹ of
each element, 0.16 % of the norm measured on a transformer and on ResNet18) takes about a tenth of
`rel`. The tolerance was not loosened for it. A gradient of the wrong type or size is rejected before
any recomputation.

Tolerances are never relaxed to let a specific result pass. The model-task rule changed on
2026-09-24 from a measurement of every engine against float64, written in ADR-0031 with what it
concedes (a bias of up to 2 % of a gradient's norm per task), not to accept one result.

## Validation on the GPU and engine checks (ADR-0031)

A validator configured with `engine = "gpu"` recomputes model tasks on its GPU (every other type
on the CPU) and states the profile in its decision; the server refuses a profile that is not
admitted as a reference for the type. Because a GPU reference shares the GPU workers' software, a
sample of what it decides (`engine_check_rate`, default 0.1, drawn from the server secret once the
GPU decision is recorded) goes to a CPU validator as an `engine_check` verification: only a
validator recomputing on the CPU is offered it, its decision must state the CPU profile, it may
belong to the same operator but not be the same validator, and it counts like an audit. A CPU
validator with `secondary_only = true` takes only audits and engine checks, never a primary
verification.

**Limit of the collective equivalence check.** It holds while the gradients stay well above their
rounding noise. Once a model has converged, Adam divides each step by the running size of the
gradient, so the relative difference between two summation orders becomes a full-size step, and the
distributed run drifts away from the single-process one. Measured on the demonstration dataset
(`cargo xtask demo --kind collective --rounds N`): worst difference ≤ 1.3e-7 up to 40 rounds,
4.0e-4 at 45, 2.3e-3 at 60; with SGD, 3.0e-8 at 60 rounds. This is not a verification failure: every
task is still recomputed and compared by the validator. It only bounds how long a distributed Adam
run can be compared with a single-process replay.

## Method for `mlp.train.v1` (ADR-0008)

1. Integrity and format of the five outputs (`w1`, `b1`, `w2`, `b2` in GGT1 with the expected
   shapes; `metrics.json` ≤ 64 KiB, log ≤ 64 entries, `steps_completed` = requested steps).
2. Declared profile admitted for this type and identical to the one recorded in `metrics.json`,
   otherwise `inconclusive` (the GPU profile is not admitted for this type: a CPU replay can diverge
   from a GPU trajectory because of a ReLU flip amplified by the optimizer; a validator of the same
   profile will be needed, P3/P6).
3. Full replay of the training run on `cpu-f32-flex-v1` (initial weights and example order fixed by
   the protocol); comparison of the four weight tensors against the tolerance.
4. `eval_loss` recomputed **from the submitted weights** and compared with the reported value; the
   loss log compared with the replay entry by entry (same schedule). A reported metric is never
   trusted.

Tested perturbations (`gg-compute/tests/mlp.rs`): modified weight, falsified evaluation loss,
forged log, insufficient number of steps, honest weights from another seed, missing output → all
rejected.

## Method for `mlp.grad.v1` and the collective run (ADR-0011)

- The validator reloads the checkpoint and the dataset (task inputs), rebuilds the micro-batch from
  the `mlp.grad.v1/order`(`round_seed`) index stream restricted to
  `[example_start, example_start+count)`, recomputes the four mean gradients on the reference
  profile and compares them within tolerance; `grad_meta.json` (`n_examples`, loss) is recomputed,
  never trusted. The tested perturbations (weights, shifted shard, another seed, modified value,
  forged metadata) are rejected (`gg-compute/tests/grad.rs`).
- The coordinator only trusts tasks `accepted` by a validator; it checks the round's coverage
  (Σ n_i = batch, no duplicated shard) before aggregating; late results or results from a canceled
  job are never incorporated (`discard-late-v1`), and a failed round is reissued with the same
  checkpoint.
- Equivalence with single-process training is not a proof of convergence: it proves that
  distribution did not change the algorithm (same effective batch, same order, same precision,
  same rules).

## Federated validation (P6, ADR-0014)

- Several validators, grouped by administrative **operator**. Quorum and audits count operators:
  several keys from the same entity do not constitute diversity.
- An `audit_rate` share of submissions is re-verified by a different operator. The draw depends on
  a server secret and happens after the result has been recorded; the worker cannot predict it and
  does not learn that it takes place.
- Combination: payment only if **all** verifications accept; `accepted` + `rejected` opens a
  dispute; `inconclusive` blocks payment and sends the task back to the queue.
- A dispute is settled by an operator, on an outcome that a validator signed, with a recorded
  reason. A disagreement is never an automatic penalty.
- What this does not provide: trustless validation, or protection against collusion between two
  operators. The gain is the detection of an isolated failure and a signed trail.

## Measuring validator quality

`gg-compute` includes a set of perturbations: exact output, noise ≤ tolerance, noise > tolerance,
one modified element, injected NaN, truncated shape, truncated file. The tests measure false
rejections (must be 0 on conforming cases) and undetected corruptions (must be 0 on non-conforming
cases).

## Cost

Reference recomputation doubles the compute cost of each accepted task. This cost is reserved
separately (the `verification_cost` of the rate schedule) and measured; it does not establish an
economic advantage for an open network.
