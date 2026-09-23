# ADR-0002 — Compute engine: Burn 0.21, Flex on CPU, WGPU/Vulkan on GPU

Status: accepted (2026-09-16)

## Context

Burn 0.22 exists only as a pre-release. In 0.21, `burn-ndarray` is marked "legacy" and the recommended CPU
backend is `burn-flex` (pure Rust, SIMD, rayon). The portable GPU backend is `burn-wgpu`; the `vulkan`
feature selects the SPIR-V compiler (type `burn::backend::Vulkan`).

## Decision

- `burn = "=0.21.0"` with features `std`, `flex`, `autodiff` everywhere; `wgpu` + `vulkan` behind the
  `gpu` Cargo feature of `gg-compute`, `gg-worker` and `gg-verifier`. The server never compiles GPU
  dependencies.
- Checked with a throwaway probe program before any layer was written: Flex matmul exactly matches a naive
  reference; an MLP built from `Param<Tensor>` + `Autodiff<Flex>` + `SgdConfig` shows a decreasing loss.
  GPU results: see `docs/STATUS.md`.
- Initial weights and data order come from our own deterministic expansion
  (`gg-protocol::drbg`, SHA-256 in counter mode) and are loaded through `TensorData`, never through a
  backend's seed.
- No LibTorch/Python dependency. cubecl-wgpu's `init_setup` panics when no adapter is found, so the worker
  probes the GPU in a subprocess (`gg-executor probe-gpu`) and fails with a clear error, with no silent CPU
  fallback.

## Consequences

The CPU and GPU numeric profiles are distinct (`cpu-f32-flex-v1`, `gpu-f32-wgpu-vulkan-v1`) and are
admitted separately, based on real tests.
