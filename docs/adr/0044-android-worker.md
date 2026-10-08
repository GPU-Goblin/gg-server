# ADR-0044 — Lending an Android phone

Status: **accepted** (2026-09-26), with the owner's answers below; step 1 waits for phones to
measure on. The owner asked for phones to compute too, and for their neural
processors (NPU) to be used where they exist. This ADR answers both: phones yes, on Android, by
their graphics processor (GPU) and within strict conditions; NPUs not now. Nothing beyond step 1
is built before its measurements are known.

## Context

- The worker computes with three engines: CPU, GPU through Vulkan (Burn on wgpu/CubeCL), and native
  CUDA (ADR-0033). Tasks are sized by the plan (ADR-0017) and cut by the lab node (rows, stages,
  tiles: ADR-0041), so a model larger than any one machine already runs as many small tasks.
- A worker announces what it can take when it claims (`WorkerCapabilities`: host and device memory,
  `gg-ops` profile, engine, kinds, whether it serves peers); tasks carry `required_ram_bytes`,
  `required_device_bytes`, `required_profile`. The claim filters on them.
- Results are accepted by recomputation or by replicas of another contributor compared within a
  tolerance (ADR-0031 norm rule, ADR-0042). The tolerance was measured between desktop engines
  (CPU, Vulkan, CUDA), never on a phone.
- The executor is confined by bubblewrap, Landlock and seccomp (ADR-0009). None of these exist for
  an Android app: an app may not create user namespaces.
- Precedent: BOINC has run volunteer science on Android phones for a decade, on the CPU, while
  charging.

What a phone is, for us:

| | Recent mid-range to high-end Android phone | Desktop with a gaming card |
|---|---|---|
| GPU | Adreno, Mali, Xclipse: Vulkan 1.1 or later on Android 10+ 64-bit devices | Vulkan, CUDA |
| Memory for a task | 1–3 GB realistic (shared with the system, killed under pressure) | 8–24 GB of card |
| Sustained speed | falls after minutes of full load (heat); a fraction of a desktop card | steady |
| Network | Wi-Fi at home; mobile data must never be used; often behind carrier NAT | fibre, UPnP |
| Running in background | only as a foreground service with a visible notification; the system may still stop it | a user service |

## Decision

### 1. Phones compute with their GPU, through the existing Vulkan engine

- The Rust worker core (`gg-client`, `gg-compute` with the `gpu` feature) is built for
  `aarch64-linux-android`. No new engine, no new kernels: the same operations as on the desktop,
  so the same catalog and the same verification.
- The CPU of a phone is not lent (little gain, much heat), except for `replica.compare.v1`
  comparisons, which are light.
- **Before anything else, a measurement** (step 1): on two or three phones (one Adreno, one Mali at
  least), run the `gg-compute` GPU conformance tests and the reference tasks, and record whether
  every operation runs (feature and limit gaps of mobile drivers: buffer sizes, workgroup sizes,
  float16/subgroup support), the agreement with the CPU reference (the ADR-0031 tolerance must hold,
  else phones may not compute accepted results), the speed, and the heat curve over 30 minutes.
  If the tolerance does not hold on a family of GPUs, that family is refused by the worker itself.

### 2. Lending only when it costs the owner nothing

The worker claims only when **all** of these hold, re-checked before each claim and every few
seconds while computing (a task running stops, and is abandoned properly, as the desktop's
"stop now", ADR-0043):
- charging, battery above a threshold (default 80 %);
- on an unmetered network (Wi-Fi or Ethernet), never mobile data;
- screen off (the owner's decision: never while the screen is on);
- battery temperature and the system's thermal status below limits (Android's thermal status and
  headroom); above them, the task stops and the worker waits to cool down;
- the owner's policy of ADR-0043 (pause, hours).

The worker announces itself as a phone (an engine `gpu:mobile` and a device-memory figure the app
measures), and claims only tasks small enough to finish in a few minutes (a new
`max_task_secs`-like bound derived from the plan's estimate), so that a stop costs little.

### 3. Confinement: Android's own sandbox, and a separate process for the task

- The executor runs in an **isolated process** of the app (`android:isolatedProcess`): its own
  user id, no permission, no network, no access to the app's files; it receives the task's inputs
  and returns its outputs through a local channel from the worker process, which alone talks to the
  network and checks every byte (hashes, signed manifests), as on the desktop.
- This replaces bubblewrap, Landlock and seccomp on Android: an **exception to ADR-0009 to write in
  CLAUDE.md** if accepted, bounded to the Android app. The inputs and outputs keep the same checks;
  what changes is how the running code is held.
- `gg-worker doctor` has an Android counterpart: it checks the process is isolated (no network, no
  file of the app readable) before the first claim, and refuses otherwise.

### 4. The application

- One app: sign in as on the desktop (e-mail and code, ADR-0030 JSON API, enrollment code fetched
  automatically), what the phone did, pause and resume, the conditions above, the fifteen languages
  of the desktop (the same translation files).
- It runs lending as a **foreground service** with a notification ("GPU Goblin is computing for
  science · Pause"); Android requires a declared foreground service type (Android 14+) and may
  still stop it. Work is scheduled with the system's constraints (charging, unmetered network,
  idle) so that the phone starts only when allowed.

### 5. Network

- Phones connect out only. They take inputs from peers and the lab node through the reversed
  connection (A8, ADR-0041) and serve nothing (`serves_peers = false`): no incoming connection, no
  upload budget on the owner's home link beyond the results.
- Downloads are counted and bounded per day (default 2 GB), shown in the app.

### 6. Verification

- Phone results are verified like any other: recomputation, or replicas of another contributor
  (ADR-0042). Until step 1 shows phones agree with desktops within tolerance on every operation,
  **a phone's replica is compared only with a desktop's**, never phone against phone, so that two
  phones of the same faulty GPU family cannot agree on a wrong result.

## NPUs: not now

- The NPUs of phones and recent PCs (Qualcomm Hexagon, Apple Neural Engine, Intel and AMD NPUs) run
  trained models in reduced precision (8-bit integers, half precision). Almost none can train a
  model, and training is most of the grid's work.
- Our verification rests on faithful recomputation in 32-bit floating point within a measured
  tolerance: reduced-precision results would not pass, and a tolerance wide enough to let them
  pass would let wrong results pass too.
- Each vendor has its own closed programming interface, changing between versions: much upkeep for
  an uncertain gain.
- They become worth it if the grid offers **inference** (running a trained model over much data:
  labelling, classification), with a verification made for reduced precision. That would be its own
  ADR.

## Consequences

- More contributors (phones outnumber gaming PCs), each small and intermittent: the plan and the
  node must cut finer, and more tasks will be abandoned (stops for heat or unplugging), which costs
  recomputation elsewhere.
- A second sandbox model to maintain and audit (Android isolated process).
- A second distribution channel (Android package signing, store rules to check before any
  submission; a volunteer-computing app must say clearly what runs on the phone).
- No effect on the desktop worker or the server beyond: the phone engine name, a task-duration
  bound in the claim, and the phone-with-desktop pairing rule of replicas.

## Plan

1. **Measure** (no app): cross-build `gg-compute` for Android, run the GPU tests and reference tasks
   on phones through `adb`: operations supported, agreement with the CPU reference, speed, heat.
   Report; decide which GPU families are allowed.
2. Headless worker for development (`adb`), claiming on a local grid with the conditions of part 2
   read from the system.
3. The isolated-process executor and its doctor; the exception written in CLAUDE.md.
4. The app (sign-in, dashboard, conditions, notification, languages).
5. Server: phone engine, task-duration bound, replica pairing rule; tests.
6. A closed beta on a few phones of the lab.

## Owner's decisions (2026-09-26)

- **Interface**: the desktop's egui interface, reused on Android (one code base, the same design and
  languages).
- **Distribution**: a signed package from gpugoblin.com for the beta, then Google Play once its
  rules are checked.
- **Minimum version**: Android 12.
- **Screen**: lending only with the screen off.
- Still open: the phones to measure on (question 1).

## Questions for the owner (as asked)

1. **Phones to test on**: which Android phones can be used for step 1 (make and model)? Without a
   real phone nothing here can be measured; there is none on this machine.
2. **Interface**: reuse the desktop's egui interface on Android (one code base, less native look), or
   a native Android interface over the Rust core (native look, a second interface to maintain)?
3. **Distribution**: Google Play, or a signed package from gpugoblin.com (and F-Droid), or both?
4. **Minimum Android version**: 12 (recent thermal and background rules, fewer old drivers) or 10
   (more phones)?
5. **Screen on**: may a phone lend while its screen is on and it is charging (a phone on a desk), or
   only screen off?
