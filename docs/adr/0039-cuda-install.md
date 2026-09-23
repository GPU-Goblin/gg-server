# ADR-0039 — Installing the native CUDA engine: a signed executor and a pinned LibTorch

Status: accepted (2026-09-25).

## Context

The native CUDA engine (ADR-0033, ADR-0038) needs an executor built against LibTorch 2.9.0 and the
LibTorch libraries on the contributor's machine. Until now the installer offered neither. Only
someone who built the worker themselves could use the engine, so no contributor could.

## Decision

- **Only the executor links LibTorch.**
  - The release adds `gg-executor-cuda`, built with `--features gg-worker/cuda`.
  - `install.sh --cuda` pairs it with the GPU build of the worker.
  - The worker process never computes. It starts without LibTorch, and the systemd unit needs no
    library path.
  - The executor finds LibTorch in its sandbox, bound read-only at `/opt/libtorch`.
- **LibTorch is fetched from PyTorch, pinned by the release signature.**
  - The archive is 3.9 GB and is not redistributed.
  - `deploy/worker/libtorch.sha256` gives the official address, the size and the SHA-256 of
    `libtorch-shared-with-deps-2.9.0+cu128.zip`. This file is part of the signed release.
  - `install.sh --cuda` checks the signature first, then downloads the archive over https from
    `download.pytorch.org` only, and refuses it unless size and digest match exactly.
  - The archive is unpacked under a directory named after its digest and reused by later installs
    of the same digest. `--libtorch-archive FILE` takes a local copy, checked the same way.
- **The digest was cross-checked when pinned.** The S3 ETag served by PyTorch
  (`96411cd9…-39`, MD5 of 39 parts of 100 MB) was recomputed from the downloaded file. It matches,
  so the pinned bytes are those PyTorch serves.
- **The engine is built and tested against that same archive.** The release workflow verifies the
  archive with the same file before building against it. The CUDA tests pass against the
  official libraries, not only against the PyTorch wheel's.
- **Configuration.** The installer sets:
  - `engine = "cuda"`;
  - `libtorch_dir`;
  - `device_memory_bytes`, if absent: 85 % of the card, from `nvidia-smi`.
- **A defect found by the rehearsal.** The cgroup probe of `gg-worker doctor` runs the executor
  outside the sandbox, with a cleared environment, and a CUDA executor could not load its
  libraries. The probe now gets `libtorch_dir`, the same libraries the sandbox binds for tasks.
  The local test grids ran without cgroups, which is why this had gone unseen.

## Consequences

**Rehearsed end to end on the development machine.** The installer ran in a temporary home, against
a release signed with a test key and served locally:

| Step | Result |
|---|---|
| Signature, digests, LibTorch archive checked, unpacked, configuration written | passed |
| `gg-worker doctor` | every check passed, CUDA initialized under the full sandbox, cgroups required |
| Second install | reused the verified LibTorch without unpacking it again |
| Archive with one byte changed | refused |
| `libtorch.sha256` changed without a new signature | refused |

- In both refusal cases nothing was installed and no configuration was written.
- The installed worker, run with its own configuration and no extra environment, trained ResNet18
  on a local grid: 3 rounds, 3 decisions accepted, 122 ms of computation per task.

**What a contributor trusts.**

- PyTorch's LibTorch binaries, at the exact digest the release signs.
- The release key, for that digest.
- The same sandbox as every other task: LibTorch runs only inside the executor's sandbox.

**Not done.**

- The release workflows are written (`gg-tools/staging`) but have not run on GitHub.
- A new LibTorch version needs a new pin (docs/RELEASE.md).
