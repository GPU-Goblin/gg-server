# ADR-0045 — Executor sandbox on Windows (AppContainer + Job Object)

Status: **accepted for the processor engine** (2026-10-05). Written before the code as ADR-0043
part 3 requires, then run on a Windows 11 virtual machine: the probe holds on every check, as an
administrator and as a standard user, and a standard user's worker computed real grid tasks in the
sandbox (STATUS.md). Still open: the graphics engines (Vulkan, CUDA) inside the AppContainer,
which need a Windows machine with a card; until measured there, Windows lends the processor only.

## Context

- The worker never computes itself: it launches `gg-executor` with typed arguments, once per task
  or as a session (ADR-0034), and talks to it through the bundle directory and stdin/stdout pipes.
- On Linux the executor is confined twice (ADR-0009): from outside by bubblewrap (namespaces, no
  network, only the bundle mounted, cgroups for memory, processes and CPU), and from inside by
  `--restrict` (rlimits, Landlock, seccomp), each measured by `gg-executor probe` and `gg-worker
  doctor`.
- None of these exist on Windows. What Windows offers an unprivileged user:
  - an **AppContainer**: a token with its own SID, low integrity, and only the capabilities it is
    given. Without the `internetClient`, `internetClientServer` and `privateNetworkClientServer`
    capabilities it has no network, loopback included. It reads and writes only objects whose
    access list names its SID or `ALL APPLICATION PACKAGES` (system files do; the user's profile,
    documents and other programs' data do not). Creating one needs no administrator rights.
  - a **Job Object**: limits on memory, number of processes and CPU rate; every process of the job
    is killed when the job's last handle closes; restrictions on the desktop (clipboard, other
    windows' handles, global atoms, display settings, logoff).
  - **process mitigation policies**, set at creation: no child processes, strict handle checks, no
    extension points, no image loaded from a remote share or with a low label.
- Chromium and Firefox confine their content processes this way. Their GPU processes are *not* in
  an AppContainer, because graphics drivers were not written for it. Whether Vulkan and CUDA work
  from an AppContainer is the main unknown of this ADR.

## Decision

### 1. The same process model

`gg-worker.exe` launches `gg-executor.exe` with the same arguments, the same bundle layout, the same
session protocol. Only the launcher and the executor's self-check change. The worker runs as the
user, from a task started at logon (not a Windows service): AppContainer profiles are per user, and
the owner's policy (away, locked, games) reads the user's session.

### 2. The launcher (worker side)

For each task, or each session:

1. **AppContainer profile** `GPUGoblin.Executor` (created once, `CreateAppContainerProfile`; its SID
   derived with `DeriveAppContainerSidFromAppContainerName`). **No capability**: no network.
2. **Access lists**, granted to the AppContainer SID only, on directories the worker created:
   - the executor's directory and the LibTorch directory: read and execute;
   - the bundle: read; its `outputs\`: read and write (a session: its root, read and write);
   - the kernel cache (ADR-0032): read and write;
   - a per-task `tmp\` inside the bundle: read and write; `TEMP` and `TMP` point to it.

   Nothing else of the user is reachable: not the profile, not the worker's key, not `control.token`,
   not other bundles. The entries are removed with the bundle.
3. **Job Object**, the process assigned to it before its first instruction
   (`CREATE_SUSPENDED`, `AssignProcessToJobObject`, then resume):
   - `KILL_ON_JOB_CLOSE`: the worker holds the only handle; if the worker dies, the executor dies
     (the equivalent of `--die-with-parent`);
   - `ACTIVE_PROCESS = 1`: the executor cannot start another process (with the mitigation policy
     below, the equivalent of the seccomp `execve`/`fork` ban);
   - `JOB_MEMORY` = the task's RAM limit (cgroups' `MemoryMax`), `CPU_RATE_CONTROL` = the owner's
     CPU share (`CPUQuota`), `DIE_ON_UNHANDLED_EXCEPTION`;
   - UI limits: no clipboard, no handles of other processes' windows, no global atoms, no desktop
     or display change, no logoff.
4. **Creation** with `CreateProcessW` and an extended attribute list:
   - `SECURITY_CAPABILITIES` (the AppContainer SID, no capability);
   - `HANDLE_LIST`: only the three pipe handles are inherited;
   - `MITIGATION_POLICY`: child process creation blocked, strict handle checks, extension points
     disabled, no remote or low-label images;
   - no console (`DETACHED_PROCESS`): a hidden console host is a child process, which the policy
     forbids, and the executor then dies in its initialization (measured);
   - a clean environment block: `SystemRoot`, `PATH` (executor directory, LibTorch `lib\`,
     `System32`), `TEMP`/`TMP`, `LOCALAPPDATA` (Windows refuses to create an AppContainer process
     without it, and points it to the container's own folder), and the variables ADR-0009 already
     sets on purpose
     (`NVIDIA_TF32_OVERRIDE=0`, `CUDA_DEVICE_ORDER=PCI_BUS_ID`, the shader cache paths, pointed into
     the kernel cache).

   `std::process::Command` cannot pass these attributes, so the launcher is its own module
   (`gg-worker/src/sandbox/windows.rs`, the only place with `unsafe` besides `restrict`), returning
   the same piped child the one-shot and session paths use today.

### 3. The executor checks itself (`--restrict` on Windows)

Before reading any input, `gg-executor --restrict` verifies what it was given, and refuses to run
otherwise (as it refuses today when Landlock is not fully enforced):

- its token is an AppContainer token (`TokenIsAppContainer`) at low integrity, with no capability;
- it is in a job whose limits include `KILL_ON_JOB_CLOSE`, `ACTIVE_PROCESS = 1` and a memory limit;
- the child-process mitigation policy is on (`GetProcessMitigationPolicy`).

It then drops what it can itself: on the processor engine, the Win32k system call policy (no
graphics subsystem at all); on GPU engines Win32k stays, the driver needs it.

### 4. The probe (`gg-worker doctor`)

`gg-executor probe` on Windows runs, inside the same sandbox as a task, and must pass all of:

| Check | Passes when |
|---|---|
| `network_isolated` | connecting to the worker's canary port on `127.0.0.1` fails |
| `internet_isolated` | connecting to a public address fails |
| `profile_unreadable` | a canary file in the user's profile cannot be read |
| `system_read_only` | writing into `%SystemRoot%` fails |
| `bundle_root_read_only` | writing into the bundle root fails |
| `outputs_writable` | writing into `outputs\` succeeds |
| `child_process_denied` | starting `cmd.exe` fails |
| `memory_limited` | allocating past the job's memory limit fails |
| `environment_clean` | only the variables set on purpose are present |
| `appcontainer_token` | the token is an AppContainer, low integrity, no capability |
| `gpu_init_restricted` / `cuda_init_restricted` | the engine starts inside the sandbox (GPU workers only) |

A worker whose probe fails does not claim tasks, as on Linux.

### 5. The graphics card

If Vulkan or CUDA do not start inside the AppContainer, GPU tasks are **not** run on Windows with a
weaker sandbox. The worker then lends the processor only, and the site says so, until a confinement
that keeps the network cut and the profile unreadable is found and measured. The probe on a Windows
machine with a graphics card decides.

### 6. The native CUDA engine (2026-10-08, the owner: "Windows must use CUDA too")

As on Linux (ADR-0033, ADR-0039): the GPU build of the worker with the CUDA executor, linked against
the official LibTorch archive for Windows, pinned by `deploy/worker/libtorch-windows.sha256` (signed
with the release, never redistributed). The installer streams it from PyTorch to disk, checks size
and digest, unpacks it into a folder named by its digest, and the sandbox grants that folder read
and execute. LibTorch's libraries need Microsoft's Visual C++ runtime: when it is missing, the
installer runs Microsoft's own installer, only once Windows finds it validly signed by Microsoft,
and Windows asks for an administrator's approval (the runtime is installed for the machine).

Measured on the virtual machine, which has no NVIDIA card: LibTorch loads inside the AppContainer
with every protection holding, and the engine reports "CUDA is not available", as it must there.
Whether CUDA computes inside the AppContainer is measured on a machine with a card, like Vulkan.

## Consequences

- No administrator rights, no driver, no service: the installer is per user.
- The sandbox is measured, not assumed: the same probe gates both systems.
- `unsafe` grows by one module (`windows-sys` calls), each call documented like `restrict`.
- The processor engine can be verified on a virtual machine; the graphics engines need real
  hardware, and stay unannounced until then.

## Plan

1. The launcher, the executor's self-check and the probe; tested on a Windows 11 virtual machine
   (processor engine).
2. The rest of the port, each piece with its test:
   - the owner's policy readings: idle and lock (`GetLastInputInfo`, session lock events), local
     time, card temperature;
   - the control token from the system's random generator, and file permissions as access lists;
   - the notification-area icon and start at logon;
   - an installer that checks the release signature in Rust (`gg-protocol::release`);
   - the Windows LibTorch archive, pinned by its digest;
   - the dual-stack listening socket for peers (`IPV6_V6ONLY` off).
3. The probe on a Windows machine with a graphics card (Vulkan, then CUDA); only then the site
   announces Windows.
