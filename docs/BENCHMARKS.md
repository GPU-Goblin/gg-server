# Benchmarks

No gain is claimed without a local baseline. Each measurement records: hardware, OS, driver,
backend, versions, precision, parameters, number of samples, spread; cold start, any JIT
compilation and warm execution are reported separately; GPU work is synchronized (`into_data`)
before timing.

Commands that name the `gg-core`, `gg-protocol`, `gg-compute` or `gg-client` crates run in a
checkout of [gg-core](https://github.com/GPU-Goblin/gg-core), those about `gg-worker` in
[gg-worker](https://github.com/GPU-Goblin/gg-worker), and the Python tests in
[gg-python](https://github.com/GPU-Goblin/gg-python); the others run here.

## Main indicator

Full cost of an accepted result = queue + transfers + execution + validation + retries + lost work.

## P0 measurements (Burn 0.21 probe, 2026-09-16)

Machine: Ubuntu 26.04, kernel 7.0.0-22, 32 threads, 62 GiB RAM, NVIDIA RTX 5060 Ti 16 GiB (driver 595.91.07, Vulkan 1.4).

| Trial | Result |
|---|---|
| Flex CPU matmul 64×64 f32 vs naive reference | difference 0 on the sampled element |
| Flex CPU MLP 4-8-1 autodiff, 20 SGD steps | loss 0.1501 → 0.1319 |
| WGPU/Vulkan (RTX 5060 Ti) matmul 64×64 and 256×512×128 vs Flex CPU | maximum difference 0 |
| WGPU/Vulkan matmul 1024³ vs Flex CPU | max abs 7.2e-5, tolerance `abs 1e-4 + rel 1e-4·|ref|` met |
| WGPU/Vulkan matmul 2048³ vs Flex CPU | max abs 1.5e-4 (beyond the absolute component alone; covered by the relative component, |ref| ≈ 15), tolerance met |
| WGPU/Vulkan matmul 2048³, synchronized timing | cold 78 ms (after kernels were compiled for the previous sizes), warm 82 ms; 1024³: cold 616 ms (compilation), warm 16.5 ms |

Command: `cargo test -p gg-compute --features gpu --test gpu -- --nocapture` (fails if the GPU is absent, does not skip).
The test also checks that the GPU result is stable across two runs (identical bytes) and, for K ≤ 512, that it agrees with a naive f64 oracle.

## P2 measurements (`mlp.train.v1`, RTX 5060 Ti vs Flex CPU, 2026-09-16)

Command: `cargo test -p gg-compute --features gpu --test gpu mlp_training -- --nocapture`.
Synthetic 16→4 dataset (teacher 32), 4096/1024 examples, batch 64, 300 steps, demo grid
(Adam, hidden 32/64 × lr 0.02/0.05 × seeds 1..3) + SGD lr 0.05 h64.

| Measurement | Value |
|---|---|
| Max absolute difference on final weights GPU vs CPU, 12 configurations out of 13 | 1.5e-7 … 2.3e-6 |
| Configuration Adam lr 0.02 h64 seed 2: max difference on the weights | **6.9e-2** (diverging trajectory; `eval_loss`: Δ 4.3e-5) |
| Max `eval_loss` difference excluding the diverging configuration | ≤ 2.0e-8 |
| Default tolerance `abs 1e-4 + rel 1e-3` met | 12/13 → GPU profile **not admitted** for `mlp.train.v1` |

Conclusion: the final weights of an f32 training run are not a robust function of the backend (a
ReLU flipping near zero, amplified by Adam, is enough); validation by replay on another numeric
profile is only reliable for computations without bifurcation points (matmul). A GPU validator of
the same profile or a trajectory-specific tolerance will be addressed in P3/P6.
First measurement (4 configurations, before the full grid): 8.5e-7 — not enough to conclude.
| Two CPU runs bit-for-bit identical (SGD and Adam, 200 steps) | yes (`tests/mlp.rs`) |
| Resuming from a checkpoint = continuous run (identical weights and log) | yes (`tests/mlp.rs`) |

## P4 measurements (`mlp.grad.v1` and collective run, 2026-09-16)

Command: `cargo test -p gg-compute --features gpu --test gpu grad_gpu_vs_cpu -- --nocapture`.

| Measurement | Value |
|---|---|
| Micro-batch gradient GPU (Vulkan) vs CPU (Flex), 4 configurations (seeds 1..4, hidden 32/64/64/128, micro-batch of 256 examples from the 16→4 dataset): max difference over the 4 gradients | **0** (bit for bit) on all 4 configurations; GPU vs CPU loss: Δ 1.5e-8, 1.5e-8, 1.3e-7, 1.5e-8 |
| Profile `gpu-f32-wgpu-vulkan-v1` admitted for `mlp.grad.v1` | yes (`profiles::GRAD_GPU_ADMITTED`) |

Collective run (`cargo xtask demo --kind collective`, 3 CPU workers under bubblewrap, 1 validator):

| Measurement | Value |
|---|---|
| 10 rounds × 4 tasks (256 examples/round, Adam lr 0.02, hidden 32, 16→4 dataset, 4096 examples) | 40/40 accepted in 32.8 s (≈3.3 s per round) |
| Computing a gradient over 64 examples in the executor | ≈50 ms (the round is dominated by claim/verification/the coordinator's 2 s tick) |
| Max difference published checkpoint vs single-process training (`gg job collective-check`) | **6.0e-8** (w1), 1.5e-8 … 4.5e-8 (b1, w2, b2) |
| Local recomputation of the 10 rounds (full batch of 256) | 0.02 s |
| In-process simulated coordinator (`tests/grad.rs`, 6 rounds × 4 tasks) vs reference | 7.5e-9 (SGD), 3.0e-8 (Adam) |

## P8 measurements (load and full cost of an accepted result, 2026-09-17)

Protocol: `GG_LOAD_WORKERS=8 GG_LOAD_TASKS=n GG_LOAD_DIM=d cargo test -p gg-control --test load
--release -- --ignored --nocapture`. Release profile, server and workers in the same process,
local PostgreSQL 17, profile `cpu-f32-flex-v1`, `audit_rate = 0`, 2 validators. Hardware: 32
logical threads. One sample per task; p50/p95 give the spread.

| Size | Tasks | Execution p50 / p95 | Verification p50 / p95 | Full cost per result | Verification overhead | Throughput |
|---|---|---|---|---|---|---|
| matmul 128³ | 64 | 0.000 / 0.001 s | 0.000 / 0.000 s | 0.001 s | 29 % | 60.9/s |
| matmul 512³ | 32 | 0.007 / 0.009 s | 0.007 / 0.008 s | 0.015 s | 89 % | 22.3/s |
| matmul 1024³ | 16 | 0.040 / 0.048 s | 0.036 / 0.038 s | 0.075 s | 88 % | 11.2/s |

Task assignment: p50 9 to 19 ms, p95 39 to 51 ms. No lost work in these three series.
The 128³ case is dominated by fixed costs and does not represent the real overhead; the other two
give the useful measurement: **an accepted result costs about twice its own computation**.
Economic interpretation and limits: docs/ECONOMICS.md.
