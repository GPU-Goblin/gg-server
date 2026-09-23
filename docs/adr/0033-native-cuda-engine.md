# ADR-0033 — A native CUDA engine: LibTorch with PyTorch's autograd

Status: accepted (2026-09-25).

## Context

On one RTX 5060 Ti, the executor on Vulkan computed a ResNet18 gradient of 32 images in 870 ms,
where PyTorch took 39 ms. Two routes through Burn were measured and do not close that gap:

- **Burn's CUDA backend.** It uses the same CubeCL kernels as Vulkan and gained at most 1.6×.
- **Burn's LibTorch backend.** It uses PyTorch's forward kernels, but computes every backward pass
  with Burn's own generic formulas. A depthwise convolution's weight gradient took 306 ms against
  3 ms in PyTorch, and a ResNet18 205 ms.

PyTorch is fast because its autograd calls cuDNN and ATen's dedicated backward kernels.

## Decision

- **An engine `cuda` / `cuda:<index>` (NVIDIA only; Vulkan stays for every other GPU).** Its
  profile is `gpu-f32-libtorch-cuda-v1`.
  - A model task runs the executor's own operators (the same `apply` as every engine) on Burn's
    LibTorch backend, **without Burn's autodiff**.
  - The trained parameters are marked as requiring a gradient on the PyTorch side, so LibTorch
    records the graph as Burn calls its functions, and PyTorch's `backward` computes the gradient.
  - One operator implementation serves every engine; nothing is written twice.
- **burn-tch is patched** (`vendor/burn-tch`, one line). It reuses the storage of a uniquely owned
  tensor for the next operation. On a tensor PyTorch's autograd tracks, that write made the
  backward pass fail. Tracked tensors are no longer written in place.
  - PyTorch detects such a write (version counters) and fails. It never gives a wrong gradient.
- **Strict float32.** By default cuDNN computes convolutions in TF32, which put a ResNet18's
  gradients 7 % away from the reference.
  - The engine runs only with `NVIDIA_TF32_OVERRIDE=0`: the sandbox sets it, and the validator's
    unit must.
  - `engine::cuda::check` refuses to run otherwise.
- **Scope.**
  - `graph.grad.v1` runs on the native path.
  - `graph.stage.v1` runs on LibTorch with Burn's autodiff for now; its backward pass uses
    PyTorch's autograd since [ADR-0038](0038-native-stage-backward.md).
  - Other kinds are refused by name.
  - Memory levers (segments, offloading) were refused on the native path; they run since
    [ADR-0038](0038-native-stage-backward.md).
- **Admission.** The profile is admitted for model tasks, as a worker's result and as a validator's
  reference. `tests/cuda.rs` checks the gradients of 13 described models against the CPU under the
  validators' rule, and of 5 captured fixtures against PyTorch's. Engine checks (ADR-0031) still
  recompute a sample on the CPU.
- **Distribution.** The executor links LibTorch 2.9.0 (the version tch 0.22 binds), built with
  CUDA 12.8, the first to know the RTX 50 series.
  - A contributor who chooses `cuda` installs the libraries in one directory: the official archive
    `libtorch-shared-with-deps-2.9.0+cu128`, 3.9 GB to download and 6.0 GB unpacked (`install.sh
    --cuda` fetches and checks it since [ADR-0039](0039-cuda-install.md)).
  - That directory is set as `libtorch_dir`, bound read-only in the sandbox at `/opt/libtorch`.
- **Sandbox.** It gets CUDA's device nodes (`/dev/nvidiactl`, `/dev/nvidia-uvm`, `/dev/nvidiaN`, no
  display node) and no address-space limit (CUDA reserves a large virtual range). Landlock also
  lets the process write under its own `/proc/<pid>/task`: the NVIDIA driver names its helper
  threads there, and its initialization failed without it. `doctor` checks CUDA under the full
  restriction (`--cuda-init`).

## Consequences

**Measured on the grid.** The grid is 1 CUDA worker, a CUDA validator, and a CPU validator taking
audits and engine checks. A round below is one without a CPU check:

| Model | Gradient in one process: Vulkan → native CUDA (PyTorch) | Round on the grid, Vulkan → native CUDA |
|---|---|---|
| ResNet18 | 870 → 130 ms (39 ms) | 5.0 → 3.5 s |
| ViT-tiny | 280 → 72 ms (37 ms) | 3.5 → 2.35 s |
| ConvNeXt-tiny | 2360 → 270 ms (224 ms) | 19.3 → 6.5 s |
| SmolLM2-135M, 2 stages | 820 → 440 ms (≈ 50 ms, 8 sequences) | 37–47 → 23–33 s |

The computation is now close to PyTorch's. What remains is around it:

- **Transfers.** Parameters in and gradients out, for every task: 370 of SmolLM2's 440 ms.
- **Starting the process.** Loading LibTorch and initializing CUDA costs about 0.8 s per task.
- **Stages** still use Burn's autodiff.

These are the next levers. Keeping parameters on the card between tasks requires a long-lived
executor, which is a separate decision.

**Costs.**

- A second numeric profile to keep admitted.
- A patched copy of burn-tch to carry until the fix is upstream.
- LibTorch as a large, NVIDIA-only dependency, for contributors who choose it.
