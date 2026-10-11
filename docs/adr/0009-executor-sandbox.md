# ADR-0009 — Executor sandbox (bubblewrap + Landlock + seccomp + rlimits)

Status: accepted (2026-09-16)

## Context

The executor processes untrusted inputs with a complex compute engine (Burn, GPU drivers). Rust's memory
safety and process separation are not enough; we must confine what the executor can reach (files,
network, processes) and what it can ask of the kernel.

## Decision

Two layers, both measured by a real probe (`gg-executor probe`, `gg-worker doctor`):

1. **Agent (bubblewrap)**: `bwrap --unshare-all --die-with-parent --new-session --clearenv`,
   read-only system (`/usr`, `/lib`, `/lib64`, `/bin`, `/etc/ld.so.cache`, `/etc/alternatives`),
   minimal `/proc` and `/dev`, `/tmp` on tmpfs, **no network** (empty network namespace, loopback only),
   the bundle mounted at `/bundle` (the only writable mount), the executor at `/gg-executor`. For the GPU:
   `/etc/vulkan` and `/usr/share/vulkan` read-only, and only the device nodes `/dev/nvidia<N>`,
   `/dev/nvidiactl`, `/dev/nvidia-uvm`, `/dev/dri` (measured: without `nvidia-uvm`, Vulkan fails with a clear error).
2. **Executor (`--restrict`, before reading any input)**: rlimits (`RLIMIT_FSIZE`, `RLIMIT_NOFILE`,
   `RLIMIT_CORE=0`, `RLIMIT_AS` = the task's RAM limit on CPU; no `RLIMIT_AS` on GPU because the driver
   reserves huge address ranges), **Landlock** (ABI v5, best effort; read-only on the system,
   `/proc`, `/sys` and the bundle; write access only to `/bundle/outputs`, `/tmp`, `/dev`), **seccomp**
   (deny list: `socket()` other than `AF_UNIX`, `execve`/`execveat`, `fork`/`vfork`, `ptrace`,
   mounts, namespaces, modules, keyrings, `bpf`, `perf_event_open`, identity changes…, returning `EPERM`).
   Socket operations (`connect`, `sendmsg`, …) remain allowed: the NVIDIA driver and the Vulkan loader use
   Unix sockets internally (measured with `strace`: denying `sendmsg`/`recvmsg` makes device creation
   fail); network isolation is provided by the empty network namespace, which also isolates abstract
   Unix sockets.
   The executor refuses to run unless Landlock is `fully_enforced`, or, since 2026-10-11 (the
   owner's decision), enforced with every right of an ABI v3 or v4 kernel (Linux 6.2 to 6.9:
   `enforced_abi_v3`, `enforced_abi_v4`). Those kernels know every filesystem right the ruleset asks
   for but `IoctlDev` (ABI v5), which the ruleset grants anyway on `/dev`, the only directory of the
   sandbox holding device nodes: the confinement is the same (`restrict::landlock_holds`, tested
   against the crate's rights per ABI). Measured on a stock Ubuntu 24.04 server (kernel 6.8):
   `enforced_abi_v4`, `doctor` passes. Older kernels (ABI v1, v2: no `Truncate`) are refused, among
   them Debian 12's 6.1.

   **AppArmor (Ubuntu 24.04).** Where `kernel.apparmor_restrict_unprivileged_userns = 1` and no
   profile covers `bwrap`, bubblewrap cannot build the sandbox ("loopback: Failed RTM_NEWADDR").
   `install.sh` then offers, and installs only with the owner's consent (sudo), the profile Ubuntu
   documents for a single program, `/etc/apparmor.d/gpu-goblin-bwrap`: `bwrap` may create user
   namespaces, as on systems without the restriction. It never adds one when the distribution has
   its own (Ubuntu 24.10 and later). Measured on the same Ubuntu 24.04 machine: refused without
   consent with nothing installed, then installed, loaded, and the whole probe holds.

The `sandbox = "none"` mode exists only with `dev_unsafe_no_isolation = true`, and never in public mode.
`gg-worker doctor` runs the probe in the real configuration: PID namespace, the agent's canary port
unreachable, `socket(AF_INET)` denied, host canary file invisible, `/usr` and the bundle root not
writable, `outputs/` writable, `exec` denied, empty environment, Landlock enforced.

3. **cgroups v2** (`cgroups = "auto"|"require"|"off"`): the agent wraps bwrap in
   `systemd-run --user --scope -p MemoryMax=<task RAM + 512 MiB> -p TasksMax=1024 [-p CPUQuota=N%]`
   when the user session delegates `cpu memory pids`; the doctor launches `gg-executor probe-cgroup`
   through the same wrapper and checks `memory.max`/`cpu.max`/`pids.max`. Otherwise it falls back to
   rlimits (with a warning).

## Not covered

- Without cgroup delegation (hosts with no systemd user session), only cooperative rlimits apply, and
  there is no `RLIMIT_AS` on GPU.
- A compromised GPU driver remains out of reach: the device nodes are exposed to the executor.
- The remaining system calls (deny list) are not an allow list.
