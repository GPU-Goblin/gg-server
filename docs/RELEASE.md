# Releasing executables

Distributed binaries are versioned and signed. A contributor runs code that talks to a control
plane and computes other people's tasks: the only thing that makes these binaries trustworthy is a
signature from a key **the contributor has chosen to trust**, verified before use, **without running
any of the downloaded files to do so**.

## What the mechanism does, and does not do

- It **produces** bytes and an unsigned manifest (`cargo xtask release`), **signs** on an offline
  machine (`gg release sign`) and **verifies** (`ssh-keygen -Y verify` + `sha256sum -c`, or
  `gg release verify`). The contributor installer (`deploy/worker/install.sh`) downloads, verifies,
  then installs; nothing else downloads or replaces executables.
- There is **no automatic update**. Nothing runs a binary on the strength of content received from
  a peer: peer-to-peer only distributes computation artifacts, never executables (ADR-0012).
- The root of trust is **the key supplied by the verifier** (the line written in the installer the
  contributor received, then pinned in `~/.config/gpu-goblin/allowed_signers`), never a key carried
  by the release. A release cannot declare itself trustworthy.

## The files of a release

| File | Made by | Content |
|---|---|---|
| `gg`, `gg-server`, `gg-verifier` | build (gg-server) | operator binaries, Linux x86_64 |
| `gg-worker`, `gg-executor` | build (gg-worker, CPU) | contributor binaries |
| `gg-worker-gpu`, `gg-executor-gpu` | build (gg-worker, `--features gg-worker/gpu`) | GPU (Vulkan) build; install.sh installs them as `gg-worker`/`gg-executor` |
| `gg-executor-cuda` | build (gg-worker, `--bin gg-executor --features gg-worker/cuda`, `LIBTORCH` = the pinned archive) | native CUDA executor; `install.sh --cuda` pairs it with `gg-worker-gpu` (ADR-0039) |
| `gg-worker-windows.exe`, `gg-executor-windows.exe` | `cargo xtask windows` (CPU) | contributor binaries for Windows 10/11 x64; the Windows installer installs them as `gg-worker.exe`/`gg-executor.exe` (ADR-0045) |
| `gg-worker-gpu-windows.exe`, `gg-executor-gpu-windows.exe` | `cargo xtask windows` (`gpu`) | Vulkan build for Windows |
| `gg-executor-cuda-windows.exe` | `cargo xtask windows` (`cuda`, against the archive pinned by `libtorch-windows.sha256`) | native CUDA executor for Windows; the installer pairs it with `gg-worker-gpu-windows.exe` |
| `libtorch-windows.sha256` | `deploy/worker/` | address, size and SHA-256 of the official Windows LibTorch archive: the Windows installer downloads it from PyTorch and refuses any other bytes. Not redistributed. LibTorch also needs Microsoft's Visual C++ runtime, which the installer takes from Microsoft when missing (signature checked by Windows, administrator's approval) |
| `gg-desk-windows.exe`, `GPUGoblinSetup.exe` | `cargo xtask windows` | the Windows application, which is also its installer (the same bytes under two names: the site offers `GPUGoblinSetup.exe`). It carries the release key of `install.sh` and the network, compiled in: it is the root of trust on Windows, as `install.sh` is on Linux |
| `libtorch.sha256` | `deploy/worker/` | address, size and SHA-256 of the official LibTorch archive: `install.sh --cuda` downloads it from PyTorch and refuses any other bytes. The archive itself is not redistributed |
| `install.sh`, `worker.toml.template`, `gg-worker.service` | `deploy/worker/` | contributor kit, covered by the same signature |
| `release.unsigned.json` | build | manifest: version, commit(s), toolchain, date, network, name/size/SHA-256 of every file |
| `release.json` | offline machine | the manifest signed in our format (DAG-CBOR, Ed25519, domain `Release`, bound to the network) |
| `SHA256SUMS` | offline machine | the same digests in `sha256sum` format, `#` lines for network/version/commit, and the digest of `release.json` |
| `SHA256SUMS.sshsig` | offline machine | OpenSSH signature (SSHSIG, namespace `gg-release`) of `SHA256SUMS`, same Ed25519 key |

Two signatures by the same key over the same content: `release.json` for `gg release verify`, and
`SHA256SUMS.sshsig` for tools present on every Linux machine. Signing both opens no cross-protocol
confusion: the SSHSIG signed blob begins with `SSHSIG` and its namespace, ours with
`gpu-goblin/v1\n` and a domain. Interoperability is tested against the real `ssh-keygen`, both ways
(`gg-protocol`, `release::tests::openssh_and_coreutils_verify_what_we_sign`; the test fails, rather
than skips, when `ssh-keygen` is missing).

## Which repository publishes what

After the split, the code lives in several repositories. **gg-server publishes the whole release**,
the contributor's binaries included: the release tooling (`cargo xtask release`) lives there and
builds the worker from a gg-worker checkout (`GG_WORKER_DIR`, or `../gg-worker`), and one manifest
per version means one offline signature per version. gg-worker has no xtask; its release workflow is
a gate (fmt, clippy, tests, CPU and GPU release builds, `shellcheck install.sh`) that creates a draft
release pointing at gg-server's release of the same version, **without attaching any binary**:
a contributor never finds unsigned executables next to the source.

The workflows are kept in `gg-tools/staging/<repo>/.github/workflows/release.yml` until the owner
puts them in place. Both run on `ubuntu-22.04` (glibc 2.35), so that the binaries also start on
Debian 12 (glibc 2.36) and Ubuntu 24.04; both strip debug info (`CARGO_PROFILE_RELEASE_STRIP=
debuginfo`: 17 MB instead of 147 MB for `gg-worker`, symbols kept); neither has any signing key.

## Publishing a version, step by step

1. **Tag gg-worker** `vX.Y.Z` (the version of the workspaces). Its gate must pass.
2. **Tag gg-server** `vX.Y.Z`. Its workflow checks out gg-worker at the same tag and runs:
   ```bash
   cargo xtask release --network gg-beta-1 --out dist     # gg, gg-server, gg-verifier, gg-worker, gg-executor
   cargo build --manifest-path gg-worker/Cargo.toml --release --bins --features gg-worker/gpu --target-dir target-gpu
   cp target-gpu/release/gg-worker dist/gg-worker-gpu; cp target-gpu/release/gg-executor dist/gg-executor-gpu
   # The CUDA executor, against the official LibTorch archive checked as install.sh checks it.
   curl -fsSLO "$(sed -n 's/^# url: //p' gg-worker/deploy/worker/libtorch.sha256)"
   sha256sum --strict -c gg-worker/deploy/worker/libtorch.sha256 && unzip -q libtorch-*.zip -d "$RUNNER_TEMP"
   LIBTORCH="$RUNNER_TEMP/libtorch" cargo build --manifest-path gg-worker/Cargo.toml --release --bin gg-executor \
     --features gg-worker/cuda --target-dir target-cuda
   cp target-cuda/release/gg-executor dist/gg-executor-cuda
   # Windows (ADR-0045): cross-compiled in Docker (cargo-xwin, pinned image), added to the manifest.
   GG_WORKER_DIR=gg-worker cargo xtask windows --network gg-beta-1 --out dist
   cp gg-worker/deploy/worker/{install.sh,worker.toml.template,gg-worker.service,libtorch.sha256} dist/
   dist/gg release add dist/release.unsigned.json gg-worker-gpu gg-executor-gpu gg-executor-cuda libtorch.sha256 \
     install.sh worker.toml.template gg-worker.service
   ```
   then creates a **draft** GitHub release with every file of `dist/`. Nothing is signed.
3. **Carry the manifest to the offline machine**: download `release.unsigned.json` (that file only:
   the offline machine attests the manifest, it never needs the binaries) onto a USB stick.
4. **Sign offline.** The offline computer has only the `gg` binary (copied once from a verified
   release) and the key stick:
   ```bash
   gg release sign release.unsigned.json --key /media/usb/release.key --out release.json
   ```
   It writes `release.json`, `SHA256SUMS` and `SHA256SUMS.sshsig` next to `--out` (the directory must
   exist) and prints the public key: check it is the published one. A missing key file is an error
   (an unmounted stick never yields a new key); a key is only created by `gg release keygen`. Read
   the printed version, commit and number of files: you are attesting them.
5. **Attach** `release.json`, `SHA256SUMS`, `SHA256SUMS.sshsig` to the draft, verify it (below), then
   publish it.
6. **Put it on the grid server** for contributors (`https://grid.gpugoblin.com/releases/`, served by
   Caddy from `/srv/gpu-goblin/releases`):
   ```bash
   sudo mkdir -p /srv/gpu-goblin/releases/vX.Y.Z && cd /srv/gpu-goblin/releases/vX.Y.Z
   sudo gh release download vX.Y.Z -R GPU-Goblin/gg-server      # or copy the assets by hand
   ssh-keygen -Y verify -f /etc/gpu-goblin/allowed_signers -I release@gpu-goblin -n gg-release \
     -s SHA256SUMS.sshsig < SHA256SUMS
   sha256sum --strict -c SHA256SUMS
   sudo ln -sfn vX.Y.Z /srv/gpu-goblin/releases/latest
   ```
   Contributors then update with the procedure of `deploy/worker/INSTALL.md`.

The very first time only: create the key on the offline machine and publish its public forms.

```bash
gg release keygen --out /media/usb/release.key
#   prints the hex key (for gg release verify) and the allowed_signers line (ssh-keygen, install.sh)
gg release ssh-key <hex>                          # the allowed_signers line, from the public key alone
ssh-keygen -lf <(echo "ssh-ed25519 AAAA...")      # its SHA256 fingerprint: the short form to publish
```

Write the allowed_signers line into `deploy/worker/install.sh` (`RELEASE_ALLOWED_SIGNERS`) and into
`/etc/gpu-goblin/allowed_signers` on the server before the first tag (`cargo xtask windows` compiles
the same line into the Windows application, and refuses to build while it is the placeholder), and publish the fingerprint
through a channel that does not depend on the repository (the lab's page, a signed message, in
person). A second copy of the key stick, kept elsewhere, avoids losing the key with the stick.

A private deployment can still sign in place with `cargo xtask release --key <file>`. The explicit
naming is deliberate: a key sitting next to the build signs anything a compromise of that machine
produces.

## Verify before running

With standard tools only (what `install.sh` does):

```bash
ssh-keygen -Y verify -f allowed_signers -I release@gpu-goblin -n gg-release -s SHA256SUMS.sshsig < SHA256SUMS
sha256sum --strict -c SHA256SUMS          # GNU coreutils and uutils skip the '#' lines
```

With `gg` (itself verified by the commands above first):

```bash
gg release verify release.json --key <public key hex published out of band> --network gg-beta-1
```

`gg release verify` rejects, in this order: an invalid signature, a manifest targeting another
network, a malformed manifest, a **revoked key**, a key **not listed** by the verifier, then any file
that is missing, has a different size or a different digest.

Rehearsed on 2026-09-24 (local HTTP server, release binaries, throwaway key used in a separate
directory standing for the offline machine): the good release installs and `gg-worker doctor` passes
(CPU, and GPU on an RTX 5060 Ti); a binary with one flipped byte is refused by `sha256sum`
("gg-worker: FAILED"); a `SHA256SUMS` signed by another key, and a `SHA256SUMS` whose digest line was
edited, are refused by `ssh-keygen` ("Could not verify signature"); a release for another network is
refused; in every refused case nothing is installed. An update to a second signed version kept the
first one next to it and the contributor's configuration, and an installer carrying another key was
refused by the pin.

Windows rehearsed on 2026-10-08 (Windows 11 virtual machine, throwaway key): `cargo xtask release`
then `cargo xtask windows --allowed-signers <throwaway>` then the kit, signed: 17 files, verified by
`ssh-keygen` and `sha256sum`; `GPUGoblinSetup.exe` downloaded from the release and run in the
user's session installed the worker, `doctor` passed, and later signed versions replaced a running
one (stopped, its folder removed) without touching the configuration. The application built
without a key refuses to install anything.

## Revocation policy

- Revocation is a decision **about the key**, not about the bytes: a valid signature from a revoked
  key is rejected. It is conveyed through the same out-of-band channel as trusted keys (project
  page, announcement signed by a key that is still valid, direct contact).
- `gg release verify --revoked <key>`, as many times as needed. Contributors using `install.sh`
  pass the new key with `--allowed-signers <file>` once they have checked it: the installer never
  switches keys by itself (the first key is pinned).
- A revocation **does not retroactively invalidate** what has already been run. It prevents
  accepting new binaries signed by that key.
- Rotation: publish the new key's allowed_signers line inside a release signed by the **old** key
  (in its `install.sh`), before revoking the old key, so that contributors can chain from one to the
  other without a break.

## Decisions to take before a public release

1. (decided, D3: dedicated offline computer, key on a USB stick never plugged into a networked
   machine) Who physically holds the computer and the stick, and where the second copy is kept.
2. How the public key is published in a way that can be verified independently of the repository.
3. Binary reproducibility: the current digests depend on the build machine. A reproducible build,
   or attestations from several builders, would be needed for a third party to verify that the
   binaries match the published code.
4. Who can revoke, and how quickly contributors are expected to refresh their list.

## Changing the LibTorch version

The CUDA executor binds LibTorch 2.9.0 (tch 0.22), on Linux and on Windows (two archives, two pins:
`libtorch.sha256` and `libtorch-windows.sha256`). A new version means, for each: download the official
archive, compute its SHA-256 and size, cross-check them against a second source (the S3 ETag served by
download.pytorch.org is the MD5 of 100 MB parts: recompute it from the file), write them in
`deploy/worker/libtorch.sha256`, run the CUDA tests against the unpacked archive
(`LIBTORCH=… LD_LIBRARY_PATH=…/lib NVIDIA_TF32_OVERRIDE=0 cargo test -p gg-compute --features cuda
--test cuda`) and rehearse `install.sh --cuda`. The file is signed with the release like every other.
