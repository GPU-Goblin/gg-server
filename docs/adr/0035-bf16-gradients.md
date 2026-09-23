# ADR-0035 — Gradients travel in bf16; weights, averages and optimizer state stay f32

Status: accepted (2026-09-25).

## Context

Once the native CUDA engine (ADR-0033) and executor sessions (ADR-0034) brought computation close
to PyTorch's, bytes became the cost of a round. Every task uploads the gradient of the parameters
it trains, the size of the model in f32. For SmolLM2-135M that is 540 MB per task and per round:
about 9 s on a 500 Mbit/s home uplink, 43 s at 100 Mbit/s.

Researchers asked for maximum precision, so the loss was measured before anything was decided
(`gg-tools/bf16grad.py`, PyTorch on the development GPU, 4 simulated workers each on its own data
shard, 2000 steps, 2 seeds). The comparison is against today's algorithm, and against a control in
which only the last bit of each f32 gradient element is changed at random:

| | Byte-level transformer, 6 layers (held-out loss) | ResNet18, CIFAR-10 (accuracy) |
|---|---|---|
| f32 (today) | 1.1368 / 1.1150 | 81.89 % / 82.10 % |
| f32, last bit changed (control) | 1.1282 / 1.1164 | 81.66 % / 82.28 % |
| bf16, rounded to nearest | 1.1247 / 1.1136 | 82.57 % / 82.64 % |
| bf16, stochastic rounding | 1.1249 / 1.1031 | 81.72 % / 82.14 % |

- **Rounding to bf16 moves a gradient by 0.16 % of its norm**, on both models; no element moves by
  more than 0.39 %.
- **Batch noise is far larger.** Two gradients of the same model on two batches differ by 21 % to
  200 % of the norm.
- **The effect on training is below chaotic divergence.** Changing one bit, 65 000 times less than
  the rounding, already moves the result as much as bf16 does.
- **Lossless compression was ruled out.** Byte planes and LZMA gain only 1.2× on these gradients,
  for 0.3 s per MB.

## Decision

- **Gradients travel in bf16 by default.**
  - This covers the `gradient` or `gradient.j` outputs of `graph.grad.v1`, and the parameter
    gradients of a `graph.stage.v1` backward pass.
  - Each element is rounded to the nearest bf16, ties to even: PyTorch's conversion, and the same
    bytes on every machine.
  - Stochastic rounding brought nothing measurable. It would also add a random draw that validators
    would have to replay.
- **What stays f32:**
  - activations (`out.k`) and their gradients (`din.k`), which have not been measured;
  - checkpoints;
  - the server's average;
  - the weights;
  - the Adam moments.
- **Format.** `GGT1` gains dtype 2 (bf16, little-endian). A reader always states the type it
  expects (`decode_as`), so a file of the other type is refused rather than converted. An element
  that is not finite, or that overflows once rounded, is refused at encoding.
- **Protocol.** The task parameters carry `gradient_bf16` and the gradient size in that type.
  - The field is absent from the encoding when false, so tasks signed before keep their bytes and
    hashes.
  - A run records its choice (`gradient_bf16`, `gradient_bf16_bytes`). Runs created before read as
    f32.
  - The shard plan keeps the f32 size of each gradient container, which is also the layout of the
    optimizer state, and adds its bf16 size.
- **Choice.** `gradient_precision = "f32"` in the run request, or in the Python package
  (`fit(..., gradient_precision="f32")`), keeps every bit. Any other value is refused.
- **Validation.** The validator recomputes in f32 and compares with the bf16 it received, under the
  unchanged norm rule (`rel` 2e-2, ADR-0031). The rounding uses up about 0.2 % of the norm, a tenth
  of the margin. The tolerance is not loosened.

## Consequences

**Measured on the grid.** The grid is 1 CUDA worker and CUDA validators; everything runs locally,
so transfers are disk-bound.

| Model | Gradient uploaded per task, f32 → bf16 | Worker upload time | Worker executor time |
|---|---|---|---|
| ResNet18 (11.2 M) | 44.7 → 22.4 MB | 181 → 88 ms | 440 → 430 ms |
| ViT-tiny (5.5 M) | 22 → 11 MB | 85 → 50 ms | 298 → 291 ms |
| SmolLM2-135M, 8 sequences | 540 → 270 MB | 1.83 → 0.95 s | 1.9 → 1.5 s |

- **Every decision was an acceptance, with no dispute.** This includes CPU engine checks, which
  recompute bf16 gradients on the CPU.
- **SmolLM2 rounds barely change on this machine:** 17.5–20.2 s in f32, 11.3–19.8 s in bf16.
  - The local grid is disk-bound.
  - The largest piece of a round is now the server's step: 8.8 s of a 16.5 s round, to read the
    weights and Adam moments, average, and write 1.6 GB back in f32.
  - That step is the next lever, not the exchange ([ADR-0036](0036-server-step-state-in-memory.md)
    brings it from 9.3 to 3.1 s).
- **On a real link the saving is the uplink time:** half of 9 s (500 Mbit/s) or of 43 s
  (100 Mbit/s) per round for SmolLM2.
- **Replays in bf16.** The grid's end-to-end tests now replay the bf16 rounding and still match the
  grid bit for bit (data-parallel, sharded, staged, tiled). A test with `gradient_precision =
  "f32"` matches the unrounded replay.

**Costs.**

- A second element type in the tensor format.
- A choice recorded per run.
- A published checkpoint is no longer bit-for-bit what an f32 run would give. It is what the same
  algorithm gives on gradients rounded to bf16, which is what the grid's tests replay.

**Not done.**

- bf16 activations between stages.
- The server-to-worker direction: checkpoints stay f32, since a worker computes from the exact
  weights.
- Local steps (several optimizer steps per task, DiLoCo), which would divide the number of
  exchanges rather than their size; discussed separately.
