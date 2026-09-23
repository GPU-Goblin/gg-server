# ADR-0032 — Compiled GPU kernels kept between tasks

Status: accepted (2026-09-24).

## Context

Each task runs in a fresh executor, inside its own sandbox (ADR-0009). On the GPU, that executor
compiles every kernel again before computing anything. On an RTX 5060 Ti, a ViT-tiny task of 32
images took 7.2 s, of which the gradient itself was 0.8 s. Two compilations are paid:

- **CubeCL**: from Burn's kernels to SPIR-V.
- **The driver**: from SPIR-V to machine code, about 4.7 of the 5.5 s of a first call.

The driver keeps its own disk cache under `HOME`, which in the sandbox is an empty tmpfs.

## Decision

- **The worker lends its executors a kernel cache directory.** It is `work_dir/kernel-cache/<16 hex
  of the executor's SHA-256>/`, bound read-write at `/kernel-cache` in the sandbox and allowed by
  Landlock. This is done only for a GPU engine: a CPU task gets no cache.
- **Two caches share that directory.**
  - CubeCL's compilation cache: the executor sets it with `--kernel-cache` before the GPU is
    initialized.
  - The driver's cache: `__GL_SHADER_DISK_CACHE_PATH` (NVIDIA) and `MESA_SHADER_CACHE_DIR` (AMD,
    Intel) point to `/kernel-cache/driver`, which the worker creates, since the driver does not.
- **Bounded, and emptied rather than trimmed.** The budget is `kernel_cache_max_bytes`, 1 GiB by
  default; 0 disables the cache.
  - Over the budget, the cache is emptied at start-up.
  - A new executor binary gets a new directory and the old ones are removed, so an update never
    reads kernels compiled by another version.

## Consequences

**Measured on the grid** (1 GPU worker, GPU validator, RTX 5060 Ti):

| Model | Worker's task, before | Worker's task, with the cache | Round without a CPU check, before | Round, with the cache |
|---|---|---|---|---|
| ViT-tiny | 6.7 s | 1.1 s | 9.0 s | 3.5 s |
| ResNet18 | 3.1 s | 1.4 s | 6.7 s | 5.0 s |
| ConvNeXt-tiny | 11.9 s | 2.8 s | 19.3 s | see STATUS.md |

The first task of each model still compiles.

**A task can now leave something behind for the next ones: the cache files.** The executor that
writes them is our own code, confined, working on untrusted data. The risk is a compromise of the
executor through its inputs that plants a malicious kernel, which later tasks would then run. What
bounds it:

- The later tasks run in the same sandbox: no network, and only their own bundle and this directory
  writable.
- Their results are still recomputed by a validator, and engine checks recompute a sample on the
  CPU.
- The directory is per worker, never shared between machines, and emptied on every update.

The risk is written in THREAT_MODEL.md. A contributor who does not accept it sets
`kernel_cache_max_bytes = 0`.

**Not done.** Entries are not signed or checked. They cannot be checked in any meaningful way: a
compiled kernel has no reference to compare with, short of compiling it again, which is the cost the
cache removes.
