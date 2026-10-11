# Status

Last updated: 2026-09-29.

This document says what GPU Goblin does today, what each claim rests on, and what is simulated or
missing. Every number below was measured by running the code; the command or test that produced
it is given next to it. Numbers come from a single development machine (one NVIDIA GeForce RTX 5060
Ti, driver 595.91.07, Vulkan 1.4, PostgreSQL 17 in Docker), so treat them as orders of magnitude,
not as benchmarks of a deployment.

**The platform is not ready for a public deployment.** Four blockers remain open: collusion between
validation operators, rotation of the server key, reproducible builds, and an independent review
(see [RISKS.md](RISKS.md)). A green `cargo xtask check` does not make a system production-ready.

Where the project goes next is in [ROADMAP.md](ROADMAP.md).

Commands that name the `gg-core`, `gg-protocol`, `gg-compute` or `gg-client` crates run in a
checkout of [gg-core](https://github.com/GPU-Goblin/gg-core), those about `gg-worker` in
[gg-worker](https://github.com/GPU-Goblin/gg-worker), and the Python tests in
[gg-python](https://github.com/GPU-Goblin/gg-python); the others run here.

## At a glance

- A researcher describes a model in PyTorch or JSON; it is validated against a published operator
  profile (`gg-ops/8`, 45 operators, integer tensors included), refused with a named reason if something is missing, and its
  cost and distribution are planned before submission.
- Workers compute in a sandbox, validators recompute, the control plane aggregates one optimizer
  step per round. The published checkpoint matches a single-process replay (bit for bit in the
  sharded, staged and tiled tests; within 1e-6 or better elsewhere).
- A model larger than any worker trains in stages; a matrix larger than a worker is tiled.
- 26 of 26 real architectures in a test corpus are captured from PyTorch and compute the same
  function at any batch size, speech models (HiFi-GAN vocoders, Wav2Vec2) and mixtures of experts
  (Mixtral, Qwen2-MoE, OLMoE, Qwen3-MoE) included; a `transformers` Llama, Mamba and Mixtral train on
  the grid unchanged.
- Ready for a private beta: hardened deployment behind HTTPS, signed worker releases verified with
  `ssh-keygen` before anything runs, researcher sign-in by ORCID or e-mail link with manual
  admission, withdrawal of a validator or an artifact, a lost worker replaced within seconds.
- Peers exchange artifacts and activations over pinned TLS 1.3, each chunk checked against a
  server-signed manifest. Nothing is paid until every required verification has accepted.

## What works

### Protocol and identities

| Claim | Evidence |
|---|---|
| Signed metadata use strict DAG-CBOR (RFC 8949 vectors, key ordering, non-canonical input rejected) with domain separation, network binding and Ed25519 (RFC 8032 vector) | `cargo test -p gg-protocol`: unit tests, versioned vectors in `tests/fixtures/protocol`, JSON round-trip tests |
| Any finite `f64` survives JSON transport unchanged, so signatures over re-encoded bodies hold | property test in `gg-protocol`; GPU test replaying real validation decisions through JSON ([ADR-0003](adr/0003-deterministic-encoding-dag-cbor.md) records the incident that motivated it) |
| Bounded tensor container `GGT1`, named-tensor container `GGB1` (sorted entries, hence a unique encoding), specified DRBG | `gg-protocol` tests; `gg-python/tests/test_tensors.py` checks that `gg data inspect` reads what Python writes |
| Optional fields added since (`shard_bytes`, `batch_inputs`) are omitted when empty | protocol vectors unchanged after each addition |

### Control plane, scheduling and ledger

| Claim | Evidence |
|---|---|
| Full path: submission, lease, execution, validation, signed receipt, single ledger write | `cargo test -p gg-control --test e2e`; `cargo xtask demo --kind matmul`: 12/12 jobs succeeded, 12/12 receipts verified offline with `gg receipt verify` |
| Credits are never reserved twice; transfers are balanced; a reward is written once even under retries | `cargo test -p gg-ledger`: 20 concurrent reservations of 15 against a budget of 100 yield exactly 6; 8 concurrent replays yield 1 write |
| Ledger is append-only; unbalanced transactions are refused at commit; `reward_key` is unique | `cargo test -p gg-storage` |
| Domain invariants: absorbing states, canceled tasks never reassigned, `inconclusive` never promoted | `cargo test -p gg-core` (including proptest) |
| An expired lease cannot be revived by a late message; a late submission gets 409 and the task is reassigned | `gg-control/tests/e2e.rs` |
| Each (task, generation) pair is assigned exactly once under load | `gg-control/tests/claim_storm.rs`: 60 workers, 200 tasks, 600 claims in 3.1 s (about 194 claims/s) |
| A worker rejected on a task is never offered that task again | scheduler test; without it, a dishonest worker could exhaust a task's three attempts |
| Tasks only go to workers that can hold them: each task carries its estimated memory and minimum operator profile; workers announce host memory, device memory, profile and engine on every claim | `gg-control/tests/collective.rs` (claim filtering test: one byte short, older profile, or no announcement yields nothing; malformed announcement yields 422); `GET /v1/researcher/grid/capacity` |
| Per-actor quotas (token bucket per token, session and IP; `429` with `Retry-After`) and Prometheus metrics with bounded labels | `gg-control/tests/quota.rs`; `GET /v1/admin/metrics` |
| Requests covered by a live lease of the caller (inputs, heartbeats, outputs and results of its active attempt; a validator's downloads for the verification it holds) consume no quota; everything else is charged, and nothing is admitted once the bucket is empty. Llama in 4 stages, one validator, 60 rounds: 2.50 s per round before, 1.68 s after, the same as with an unlimited quota | `gg-control/tests/e2e.rs` (`requests_covered_by_a_lease_do_not_consume_the_quota`) |
| Resumable downloads (`Range`, 206/416) on both sides, hash computed on the bytes actually received | `gg-client/tests/download.rs` (server cuts the connection a third of the way through) |
| Backup and restore | `cargo xtask db backup\|restore`; `gg-control/tests/restore.rs`: receipts still verify and the ledger is still balanced after restoring into a fresh database |
| Deadlocks and serialization failures in PostgreSQL (40P01, 40001) are retried for decisions, submissions and the coordinator, then answered with 503 | `gg-control/tests/retry.rs` provokes a real deadlock |
| Audit rate (default 0.25) changes while the server runs, and the change is audited | `gg-control/tests/federated.rs`; `gg admin audit-rate` |
| No project is created without explicit acceptance of the data policy (synthetic and public data only) | dedicated test; `data_policy` published by `/v1/info`; `--ack-data-policy` |

Researcher authentication uses ORCID (server-side Authorization Code flow, single-use `state` bound
to a login cookie, `HttpOnly`/`SameSite=Lax` sessions, CSRF protection, `pending` admission until an
operator approves, API token shown once, revocation). `gg-control/tests/portal.rs` covers the flow
against the simulated provider, and the real provider code path is tested against a local mock
token endpoint. **The real ORCID Sandbox has never been tested**: no client credentials were
available ([ADR-0010](adr/0010-orcid-authentication.md)).

### Execution and sandbox

`gg-worker` runs `gg-executor` inside bubblewrap (user, mount, PID, network, IPC and UTS
namespaces; no network; read-only system; empty environment; only the bundle writable). Before
reading any input, the executor applies Landlock (ABI v5, fully enforced), a seccomp filter
(sockets other than `AF_UNIX`, `execve`, `fork`, `ptrace`, mounts and modules return `EPERM`) and
rlimits ([ADR-0009](adr/0009-executor-sandbox.md)). `gg-worker doctor` runs an escape probe in the
real configuration:

| Probe | Measured result |
|---|---|
| PID namespace | pid 2, 2 visible processes |
| Network | agent canary port unreachable (`EPERM`), loopback only |
| seccomp | `socket(AF_INET)` returns `EPERM`, mode 2 |
| Mounts | host canary file invisible, `/usr` read-only, bundle root not writable, `outputs/` writable |
| Processes | `exec /bin/true` refused |
| Landlock | fully enforced, `no_new_privs` |
| cgroups (systemd user scope) | `memory.max = 268435456`, `cpu.max = 50000 100000`, `pids.max = 64` |

`gg-worker/tests/sandbox.rs` (requires `bwrap`) runs the full probe, checks that the layers are
detected as absent without `--restrict`, and runs a restricted matmul with exact output. GPU
execution works under the same restrictions with only the NVIDIA and DRI device nodes exposed;
`doctor` initializes the GPU inside the sandbox and refuses to start if the adapter is missing or is
a CPU implementation (no silent fallback). All reference demos run with `sandbox = "bwrap"`.

The executor itself refuses a task whose estimated memory exceeds the declared device memory
(`gg-executor run --device-memory N`), and checks the measured peak afterwards.

### Validation

No acceptance relies on metrics reported by the worker: the validator recomputes.

| Claim | Evidence |
|---|---|
| Matmul verification: 0 false rejections on 20 conforming cases, 0 undetected corruptions on 10×10 perturbed cases | `cargo test -p gg-compute --test perturbations` |
| `mlp.train.v1` verified by full replay: loss decreases, two CPU runs are bitwise identical, an interrupted run resumed from its checkpoint gives the same weights and log; six perturbations rejected; an unadmitted profile yields `inconclusive` | `cargo test -p gg-compute --test mlp` |
| `mlp.grad.v1` and `graph.grad.v1`: tampered gradient rejected (by comparison and by hash), false declaration refused before any computation, unadmitted profile yields `inconclusive` | `gg-compute/tests/grad.rs`, `gg-compute/tests/graph_task.rs` |
| GPU profile `gpu-f32-wgpu-vulkan-v1` admitted for `tensor.matmul.v1` (max deviation 1.5e-4 at 2048³), `mlp.grad.v1` (bitwise identical to CPU on 4 configurations) and `graph.grad.v1` (by norm since catalog 2, `‖a − b‖ ≤ 2e-2·‖b‖ + 1e-7·√n`, ADR-0031, measured against float64 on ResNet18) | `cargo test -p gg-compute --features gpu --test gpu`, including gradients GPU against CPU on 12 described models (`graph_gradients_gpu_vs_cpu`); the tests assert that a CPU validator accepts the GPU result, so a regression revokes admission |
| A validator may recompute model tasks on the GPU (`engine = "gpu"`); a sample of what it decides is recomputed again on the CPU (engine check); the server refuses a reference profile not admitted for the type, and a GPU profile on an engine check | `gg-control/tests/collective.rs` (`a_gpu_verified_result_waits_for_its_cpu_engine_check`: nothing aggregated before the CPU check, refused GPU profile, a CPU disagreement opens a dispute); `gg-compute` `profiles` and `compare` unit tests; real grid above |
| GPU **not** admitted for `mlp.train.v1`: one trajectory in 13 diverges (6.9e-2 on the weights), so such results are `inconclusive` and never paid | [BENCHMARKS.md](BENCHMARKS.md) |

**Federated validation** ([ADR-0014](adr/0014-federated-validation.md)). Quorum counts operators
(administrative entities), never keys. After a submission is recorded, the server draws an audit
from a local secret; the audited submission is re-verified by a validator of another operator, and
nothing in the responses to the worker reveals the draw. A disagreement opens a dispute: no payment,
no sanction, no requeue, until an operator settles it on an outcome that a validator actually
signed.

| Claim | Evidence |
|---|---|
| Audited submission paid only after two distinct operators; two keys of one operator count once; dispute settlement refused on an unsigned outcome (422) and settled only once (409) | `gg-control/tests/federated.rs` |
| Audit draws are deterministic given the secret, differ across secrets, and follow the rate (about 500 of 2000 at 25%) | unit tests in `workflows::federated` |

**The public site gpugoblin.com** (`web/`, Zola; deployed 2026-09-26 on the shared server with
`deploy/server/nginx-gpugoblin-site.conf`). Ten pages in fifteen languages (English and French
written and read by hand; the thirteen others machine-translated, each checked by
`web/check_lang.py`: same keys, placeholders, untranslated values, links and code blocks, and a
build). Contributor sign-in and account on the site through `/contributor/v1` proxied under the
site's own address (no cross-site request); researchers are sent to the grid's portal. SEO: titles,
descriptions, canonical URLs, hreflang between the fifteen versions, sitemap, Open Graph image,
JSON-LD (Organization, WebSite, WebPage, FAQPage, SoftwareApplication); for language models
`llms.txt` and `llms-full.txt`, and `robots.txt` naming the AI crawlers as allowed. Strict CSP (only
the site itself), no cookie, no tracking. Legal notice and privacy policy (French authoritative).
Downloads are shown as "coming with the first signed release" until a signed release exists
(`releases_published` in `web/config.base.toml`). Sign-up answers "not open yet" until the mail
relay is configured.

**The owner's control of a worker, and the application** ([ADR-0043](adr/0043-contributor-desktop-app.md),
parts 1 and 2, Linux). The Windows port (part 3, [ADR-0045](adr/0045-windows-sandbox.md)) is
in progress and **has not run on Windows yet**: see the paragraph below. The application has an About page (Nicolas Michel — LAAS-CNRS, contact@gpugoblin.com,
thanks, licence), a console mode for servers, and drives one worker per graphics card. At start
it offers to sign in or create an account (e-mail code, JSON contributor API) and then installs
without copying a code; it speaks fifteen languages (machine translations, to be read by native
speakers; no Arabic or Urdu: egui has no right-to-left layout).

| Claim | Evidence |
|---|---|
| A policy (pause, only when idle, hours through midnight, GPU temperature, yielding the card to a game, memory) decides whether a task is claimed; windows and gates checked | `gg-worker` `control` unit tests |
| The control API listens on loopback only, refuses without the token (constant-time check), pauses (immediately: the running task is told to stop), writes the policy back, refuses an impossible one | `gg-worker/tests/control.rs` |
| The real agent asks the server for nothing while paused, asks again once resumed, stops again when paused; it reports the credits and accepted computations the server counts | `gg-worker/tests/policy.rs` |
| On a local grid: "pause now" during a 2048³ product abandoned it, the worker waited, resumed, and all 5 tasks were accepted | by hand, `cargo xtask demo --kind matmul --dim 2048 --jobs 5 --workers 1` with the API driven by `curl` |
| `gg-desk`, the application: its link finds the worker through the files it writes, reads its status, pauses, writes a policy, shows a refusal, resumes; the enrollment code is checked before the installer runs; days of a window toggle back to "every day" | `gg-desk/tests/link.rs`, `gg-desk` unit tests |
| The application against a worker of the local grid: computing, then waiting because the session is in use (only when away); the settings show the worker's policy | pictures with `gg-desk --screenshot` during `cargo xtask demo --kind matmul --dim 2048 --jobs 5 --workers 1` |
| Several cards: the application finds a worker in the data directory and in `gpu1`, labels them by card, pauses one while the machine lends, then both; the console without a display resumes one card, prints JSON, writes a policy to both, refuses an absent card and an impossible value without changing anything, exits 2 with no worker | `gg-desk/tests/link.rs` (two real control APIs, the `gg-desk` binary); `console` unit tests (schedules, `set`) |
| `install.sh --gpu --gpus 3`: three configurations (engines `gpu`, `gpu:1`, `gpu:2`; loopback ports 7460 and free ones; data directories `gpu1`, `gpu2`; names), `init` and `doctor` for each, the `gg-worker@.service` template, `gg-desk` for the terminal only without a desktop | by hand: the installer run end to end against a release signed by a throwaway key, stand-in binaries, `--no-start`; the real `gg-worker` loads the generated `worker-gpu1.toml` |
| The contributor account as JSON: code sent (422 for a bad address), wrong code 403, session, a used code refused, `me` without or with a wrong session 401, enrollment code, a machine attaches itself with it and appears in `me`, the page's cookie shows the same account, sign-out; 404 while sign-up is closed | `gg-control/tests/contributors.rs` |
| The application's languages: every text translated in all fifteen, nothing else, placeholders kept, a glyph for every character; the session file is 0600; only https (or this machine) is accepted as a grid | `gg-desk` unit tests (`i18n`, `account`, `welcome`); pictures of the welcome screen in English, Hindi and Chinese |
| The sandbox numbers CUDA cards in bus order (`CUDA_DEVICE_ORDER=PCI_BUS_ID`), and `doctor` accepts that variable inside | `gg-worker` `sandbox` unit test |

**Windows port** ([ADR-0045](adr/0045-windows-sandbox.md)). Built on this Linux machine for
`x86_64-pc-windows-msvc` with cargo-xwin in Docker (`gg-tools/xwin.sh`), run on a **Windows 11
Enterprise evaluation virtual machine** (build 26300, QEMU/KVM through `dockurr/windows`, 4 cores,
8 GB, no graphics card; scripts in `gg-tools/winvm/`). The processor engine works there; the
graphics engines (Vulkan, CUDA) have not run on Windows, which needs a real machine with a card.
Windows is not announced on the site (`windows_available = false`).

| Claim | Evidence |
|---|---|
| The sandbox holds on Windows 11, as an elevated administrator and as a standard user: AppContainer token without capability at low integrity; a job of one process with a memory limit, killed with the worker; loopback and the internet unreachable; the user's files unreadable; the system and the bundle root read-only, outputs writable; another program blocked by Windows; memory past the limit refused; only the variables set on purpose | `gg-worker/tests/sandbox_windows.rs` (4 of 4 on the VM), `gg-worker doctor` in public mode: "agent may start" |
| The executor refuses to compute when started outside the sandbox; the worker refuses an elevated token (as root on Linux) and a program folder the user does not own, with the reason | `sandbox_windows.rs` (`outside_the_sandbox_the_executor_refuses…`), `doctor` as administrator and as a standard user |
| A real grid: the Windows worker of a standard user, in the AppContainer, registered, was authorized and computed 6 of 6 matmul tasks, all accepted after the validator's recomputation; ledger balanced | `gg-tools/winvm/e2e.sh` (`cargo xtask demo --kind matmul --jobs 6 --workers 0 --external-workers 1`, worker in the VM through a relay on the Docker bridge): "demo OK" |
| Problems found only by running on Windows, fixed: no `VCRUNTIME140.dll` on a fresh Windows (the C runtime is now linked in); `LOCALAPPDATA` required by Windows to create an AppContainer process; the hidden console host of `CREATE_NO_WINDOW` is a child process, which the policy forbids (`DETACHED_PROCESS` now); each task rewrote the access lists of the program folders (now only when missing) | commits `32fd463`, `acaad35` |
| Tests of the worker and the application on Windows: peer exchange in dual stack (13), the owner's policy with the real worker (pause), the control API, enrollment, NAT-PMP, UPnP, the application's console with two cards, installer decisions, launcher quoting and environment | the test executables of `gg-worker` and `gg-desk` run on the VM: all pass but `i18n::every_language…`, which reads the source tree and has no sources there (it passes on Linux) |
| Linux unchanged by the port: tasks and executor sessions start through one `Launch`; the agent's `Session` answers a task in the real bubblewrap sandbox | `gg-worker/tests/sandbox.rs`, `cargo xtask demo --kind matmul` |

| The application installs, starts, lives in the notification area and is removed, in a real Windows session (UAC on, administrator not elevated), from a test release signed by a throwaway key: `GPUGoblinSetup.exe` opened from Explorer asks for no elevation (manifest `asInvoker`); signature and digests checked, version installed side by side, older versions removed, doctor passed, start at logon (the application came back after a reboot), Start menu entry and entry in the list of installed apps with the program's icon; the tray icon with its tooltip, opening the window, its menu (pause, stop now, resume greyed while lending, quit); `gg-desk uninstall` asks twice, keeps the key and settings when told to, removes the programs folder, the start at logon, the Start menu entry and the list entry | by hand on the VM: `gg-tools/winvm/make-release.sh`, `gg-desk install --server-url http://127.0.0.1:18080` in the user's session, screenshots (`shot.sh`) and clicks (`vm.sh`) |
| Problems found only in a Windows session, fixed: without a graphics driver OpenGL 2 is missing and the window could not open (drawn with Direct3D 12 now, which falls back to Windows' software renderer); a program named like an installer is elevated by Windows (manifest); the uninstaller left its own program behind (it now waits for its process to end); the installer's output redirected to a file was lost; the taskbar showed a generic icon | commits `29018fa`, `fb96f9d` and the one before |

| A release carries the Windows files: `cargo xtask windows` builds them (cross-compiled in Docker, image pinned by digest, or native on Windows) with the release key of `install.sh` compiled into the application, and adds them to the unsigned manifest; it refuses while `install.sh` holds the placeholder key. Rehearsed end to end with a throwaway key: 17 files signed and verified by `ssh-keygen`/`sha256sum`, `GPUGoblinSetup.exe` downloaded from the release and run on the VM, worker installed, `doctor` passed; a later version replaced a running one | `cargo xtask release` + `cargo xtask windows --allowed-signers …` + `gg release sign`, docs/RELEASE.md |
| Version 0.1.0 built and signed with the real release key (created 2026-10-11 on the build machine, not offline yet; fingerprint `SHA256:673XukOsx9kY85im+4qIA2IRxZU3+bEFVaD2VZwyBcM`, pinned in both installers and compiled into the Windows application): 22 files, Linux binaries built on Ubuntu 22.04 (glibc 2.34 at most) and the Windows files, network `gg-beta-1`, commit a180a4e. Verified by `ssh-keygen`/`sha256sum` and `gg release verify` (another network refused); `install.sh` taken from the release and run in an isolated home against a local server: CPU, GPU (Vulkan) and CUDA installs pass `doctor` on the RTX 5060 Ti (this machine has the AppArmor user-namespace restriction off). **Published on 2026-10-11** at `https://grid.gpugoblin.com/releases/v0.1.0` (`latest`), verified on the server against `/etc/gpu-goblin/allowed_signers` and again from outside; the grid runs that release's `gg-server`, contributor sign-up is open, the first validator (`server-validator`, operator "GPU Goblin", CPU, 2 cores and 4 GB at most) runs on the server, and the site offers the Linux downloads with the key's fingerprint | `gg-tools/release-build/` (build-linux.sh, rehearse.sh), docs/RELEASE.md |
| A stock Ubuntu 24.04 can lend (2026-10-11, measured in a virtual machine from Ubuntu's own cloud image, kernel 6.8, AppArmor restriction on): bubblewrap is refused there until a profile allows it; `install.sh` explains, refuses without consent with nothing installed, and with consent installs `/etc/apparmor.d/gpu-goblin-bwrap` (never next to a distribution's own profile), after which the whole probe holds. Landlock on kernels 6.2 to 6.9 lacks only `IoctlDev`, granted on `/dev` anyway: accepted as `enforced_abi_v3`/`v4` (ADR-0009), `doctor` passes there. **Not in a release yet**: 0.1.0's executor refuses that kernel and its `install.sh` has no AppArmor step. Debian 12 (kernel 6.1, Landlock v2) stays refused | `restrict::linux` tests, `gg-tools/ubuntu-vm/` (run.sh), ADR-0009, `deploy/worker/INSTALL.md` |

| The native CUDA engine on Windows: the CUDA executor built for Windows against the pinned official LibTorch (cross-compiled, it imports `torch_cuda.dll`); the installer downloaded LibTorch from PyTorch inside the VM (2.9 GB, size and SHA-256 checked, unpacked by digest, reused by the next version), installed Microsoft's Visual C++ runtime after Windows' approval (its signature checked first), wrote the CUDA configuration; LibTorch loads inside the AppContainer with every protection holding; without an NVIDIA card the engine reports "CUDA is not available" and doctor refuses, as it must | the VM: `gg-desk install --cuda`, doctor; `cargo xtask windows` |

Not done: signing the Windows programs themselves (Authenticode: SmartScreen warns about an unsigned download); router port mapping (no default gateway reading on Windows); AMD/Intel card temperature on Windows; the tray menu from the keyboard (only the mouse opens it); Vulkan and CUDA computing inside the AppContainer (real hardware with a card).

**Verification by other contributors** ([ADR-0042](adr/0042-peer-verification.md), steps 1 to 5,
`replica_verification`, off by default). Each task is computed again by a worker of another
contributor account, drawn by the server, and the two results are compared instead of being
recomputed by a validator.

| Claim | Evidence |
|---|---|
| Two accounts that agree are both accepted and paid (primary and replica receipts), with no validator verification; another worker of the first account is not offered the replica; the replica's assignment says `primary` | `gg-control/tests/replicas.rs` (`two_contributors_that_agree_are_both_paid`) |
| A replica that lies calls a third computation by a third account; the majority is accepted and paid, the liar rejected and not paid, its task cancelled | `a_replica_that_lies_is_outvoted_and_not_paid` |
| The replica goes first to the worker the server drew, whoever asks first | `the_replica_goes_first_to_the_worker_the_server_drew`; the draw's stability and independence in `workflows::replicas` unit tests |
| Three computations that all differ open three disputes; nothing paid until an operator settles one | `three_computations_that_all_differ_go_to_a_human` |
| The comparison: both ways, within the task's tolerance, JSON metadata without the machine's profile | `gg-compute` `replica` unit tests |
| A job a lab node holds: the node compares; its agreement is taken; a difference it finds counts once a comparer of a foreign account finds it too, then a third computation settles the task; a node that claims a difference two comparers do not find is suspended and both computations accepted; a comparer contradicted by a second has its account suspended. The server reads no output | `gg-node/tests/replicas.rs` |
| Spot checks and probation: a drawn (or probation) task has its third computation from the start and waits for it; three that agree are all paid; a pair that returns the same wrong result, contradicted by the check, has both accounts suspended (their machines get no more work) and goes to disputes; the draw follows the rate (about 100 of 2000 at 5 %) | `gg-control/tests/replicas.rs`, `workflows::replicas` unit tests |
| A comparer's verdict can be checked exactly by comparing again (a lying verdict rejected) | `gg-compute` `replica` unit tests |
| A worker drawn for a replica but busy (or whose account has since computed another computation of the task) does not hold it: another account takes it at once. Found by the demo: a 30 s stall per round (graph run 124 s instead of 4 s) | `a_busy_drawn_worker_does_not_hold_the_replica` (fails without the fix) |
| Real binaries, replicas on, probation making every task three computations: `cargo xtask demo --replicas` (MLP grid 4.0 s), `--kind matmul --replicas` (3.5 s), `--kind matmul --node --replicas` (2.8 s), `--kind collective --replicas` (4.9 s), `--kind graph --replicas` (3.9 s, against 3.4 s with a validator), `--kind graph --node --replicas` (5.0 s); one receipt per accepted computation, spent = rewards (no validator's recomputation charged) | demo reports |
| `cargo xtask demo --kind matmul --federated` | 6/6 receipts cite two signed decisions from two named operators |
| `cargo xtask demo --kind matmul --federated --faults` | a third validator accepts without recomputing: 3 disputes opened and settled as `rejected`, no corrupted result paid, 14/14 checks |

**Fault injection** (`--faults`) covers a worker killed while holding a lease, a dishonest worker
that corrupts outputs and replays submissions, an executor killed after its first checkpoint
(local resumption), and a round canceled mid-execution. In every variant, corrupted results are
rejected and never paid, replays are idempotent, and the final checkpoint remains equivalent to
the reference.

### Reward commitments (off-chain)

A batch commits to an immutable list of rewards through a Merkle tree defined once in `gg-merkle`
and shared by the control plane and the contract ([ADR-0015](adr/0015-reward-settlement.md)). Each
leaf binds network, chain, contract, batch, reward id, beneficiary, asset, amount and receipt hash.

| Claim | Evidence |
|---|---|
| Length-prefixed encoding, domain-separated leaves and nodes, odd leaf promoted (not duplicated), empty batch and duplicates refused | `cargo test -p gg-merkle` (shared vectors) |
| Contract pays the committed beneficiary once, even when a third party relays or the same obligation is recommitted in a second batch; proofs from another leaf, contract, chain, network or asset are refused; never pays more than it holds | `cargo test --manifest-path contracts/gg-rewards/Cargo.toml` (simulated chain, `cw-multi-test`) |
| Wasm artifact (345 KiB) exports the CosmWasm entry points | checked by `cargo xtask check` |
| Batches built from accepted receipts; contributions without a declared payout address are never committed; settlement state machine `prepared → submitted → observed → finalized` with `to_reconcile` | `gg-control/tests/rewards.rs` |

A batch root commits to a list; it proves no computation. **Nothing is deployed, not even to a
testnet, and no real funds are involved.**

### Models described as graphs

| Claim | Evidence |
|---|---|
| Format `gg-graph/v1` with shape inference and derived execution order; tabular MLP, vision CNN, transformer block (17.2 M parameters) and time series described in JSON | `cargo test -p gg-compute --test graph`; examples in `examples/models/` are extracted from the tests |
| Named refusal before any reservation: unknown operator, mismatched shape, missing tensor, cycle, unknown or missing attribute, exceeded bound, unknown field anywhere in the description; exit code 2, no server needed | `gg job check-model` |
| The profile the server publishes is exactly the one local validation runs against | `GET /v1/catalog/ops`; end-to-end test in `gg-control/tests/e2e.rs` |
| Cost estimate before submission: parameters, bytes, FLOPs per example, memory for a batch | transformer block: 65.5 MiB of parameters, 6.95 GFLOP per example, 1.28 GiB for a batch of 64 |
| Task memory estimate bounds the executor's measured usage from above (worst case 2.26×) over 5 models × 2 batch sizes × 4 segmentations, with and without offloading | `cargo test -p gg-compute --test graph_memory_estimate` |
| Execution with Burn, forward and backward by autodiff: a described MLP matches the `mlp.grad.v1` kernel (≤ 1e-6); finite differences on a CNN and a transformer; two runs byte-identical | `cargo test -p gg-compute --test graph_exec` |
| GPU vs CPU per operator: max absolute deviation 1.2e-5 (Gelu); GPU reproducible with itself byte for byte | `cargo test -p gg-compute --features gpu --test gpu` |
| Profile evolution: causal mask (`gg-ops/2`, [ADR-0020](adr/0020-causal-mask.md)), positions and `Slice` (`gg-ops/3`, [ADR-0021](adr/0021-positional-encoding.md)), scaled RoPE (`gg-ops/4`, [ADR-0022](adr/0022-scaled-rope.md)), `ReduceMean` and `Rsqrt` (`gg-ops/5`, [ADR-0023](adr/0023-hugging-face-models.md)), `Formula` and description functions (`gg-ops/6`, [ADR-0025](adr/0025-custom-operators.md)) | see below |

**Causal mask.** Changing token `j` changes no position `< j`, byte for byte, in direct and
segmented execution, on CPU and GPU; the same test without the mask detects the leak. Trained 250
Adam steps on random tokens (floor ln V = 3.466), a model without the mask reaches 2.734 by reading
the future; with the mask it stays at 3.478 (`gg-compute/tests/graph_causal.rs`).

**Positions.** On "copy the token two positions back" (floor 2.773), the causal mask alone reaches
1.360, learned positions 0.001, RoPE 0.253 (`gg-compute/tests/graph_positions.rs`). After rotation,
the dot product at `(m, n)` equals that at `(m+s, n+s)` within 1e-5.

**Scaled RoPE.** A rotary table is described by explicit frequencies and an amplitude; the executor
knows one formula, `A·cos(t·ω)`, and no variant by name. Linear, Llama 3.1, YaRN, LongRoPE and
dynamic NTK each keep their defining property (`gg-compute/tests/graph_rope_scaling.rs`). Against
`transformers` 5.17, 14 of 16 configurations are byte-identical and 2 differ by 1 and 2 ulp
(`gg-python/tests/test_rope_scaling.py`). Over 2,048 positions, Rust tables match `transformers` for
95.0% of values at base 10,000 (max 6e-8) and 90.6% at base 150,000 (max 1.5e-5).

### Collective and staged training

The control plane holds the official training state (weights and Adam state). Each round issues
tasks on disjoint, explicit slices of the round's examples; once all are accepted, one transaction
checks coverage, aggregates `Σ n_i·g_i / Σ n_i`, applies one optimizer step and publishes the next
checkpoint, keyed by `(run, step)` so a step is never aggregated twice. A failed or canceled round
is reissued; late results from a canceled job get 409 and are never integrated
([ADR-0011](adr/0011-collective-training.md)).

| Claim | Evidence |
|---|---|
| Collective MLP run equals the single-process reference | `cargo xtask demo --kind collective`: 10 rounds × 4 tasks, 3 sandboxed workers, 40/40 accepted in 32.8 s; `gg job collective-check` recomputes all rounds and measures a max deviation of 6.0e-8 |
| Same with faults: canceled round reissued, fragile worker's held result never submitted or paid, killed worker's lease reassigned | `cargo xtask demo --kind collective --faults`: 10/10 rounds, 40 receipts, final checkpoint equivalent at 6.0e-8 |
| A described graph trains collectively | `cargo xtask demo --kind graph`: 6 rounds, 24/24 tasks accepted, deviation 0 from the reference |
| Sharded checkpoints: at most 40 shards of at most 1 GiB (40 GiB per checkpoint), deterministic from the description, aggregated shard by shard; frozen shards never rewritten | `gg-compute/tests/graph_shards.rs`; `gg-control/tests/collective.rs`: whole, sharded, and sharded with parameter-by-parameter initial weights give bitwise-identical published weights |
| Server-provided micro-batches from row-sharded datasets | `gg-control/tests/collective.rs`: bitwise-identical weights; each task received 8 rows instead of 96 |
| Memory levers executed in the executor: activation recomputation (segmented execution), parameter offloading, frozen parameters | `gg-compute/tests/graph_memory.rs`: gradient deviation 1.8e-5, decreasing to 1.25e-5 with 100 segments; offloading gives byte-identical gradients; a frozen parameter is unchanged after 3 distributed rounds ([ADR-0019](adr/0019-when-a-stage-does-not-fit.md)) |
| Staged execution: chaining stages reproduces the gradient byte for byte (transformers and a real Llama, 2, 3 and 5 stages) | `gg-compute/tests/graph_stages.rs`, `gg-compute/tests/graph_stage_task.rs` ([ADR-0024](adr/0024-staged-execution.md)) |
| A model larger than every worker trains in stages | `gg-control/tests/collective.rs`: three workers with a third of the model's memory, 48 verified tasks, checkpoint bitwise identical to the replay |
| A matrix larger than every worker is tiled (`MatMul` columns, `nn.Linear` rows, embedding columns) and spread over stages | `gg-compute/tests/graph_tiling.rs`, `graph_tiling_auto.rs`; `gg-control/tests/collective.rs`: server rewrites the description, reassembled checkpoint bitwise identical |
| Speculative waves: the next wave opens on submitted outputs; if a forwarded output is rejected, the round is poisoned and reissued | `gg-control/tests/collective.rs`: a forged activation read by the next wave before any decision is rejected, the round reissued, and the checkpoint is byte-identical to an honest run ([ADR-0026](adr/0026-speculative-waves.md)) |
| Hand-off before the durable copy: an activation can be served before it is stored; its verification is held until the copy arrives, and a copy that never arrives reissues the round | `gg-control/tests/collective.rs` (two tests) |
| Description functions are inlined by the server (source kept) and train on the grid | `gg-control/tests/collective.rs`: checkpoint equal to the replay within 1e-6 |

**Staged training on the real grid** (3 sandboxed workers, validators, `LlamaForCausalLM` from
`transformers`): workers announcing 1.96 MB while a whole-model task needs 5.87 MB; the plan
answers "fits in 4 stages" and `Trainer.fit()` uses it with nothing else: accuracy 5.4% → 92.2%,
loss 2.78 → 0.26, in 40 rounds.

**Cost of stages** (same Llama, 2 layers, 10 rounds of 128 examples in 4 tasks, before the D7
speed work below):

| | Data parallelism | 4 stages |
|---|---|---|
| Memory per task | 5.87 MB | 1.93 MB (÷ 3) |
| Round duration | 2.0 s | 21.7 s (× 10.8) |
| Tasks, hence verifications | 40 | 320 (× 8) |
| Bytes stored | 5.8 MB | 30.7 MB (× 5.3) |

Verification scales with computation (each stage is recomputed once), with no hidden factor.

**Pilot models on the real grid** (2026-09-24; 4 sandboxed CPU workers with the default 4 GiB, 2
validators, peer-to-peer on, release build; `transformers` models at their real size, synthetic
data, 32 examples per round in 4 tasks; `Trainer.fit()` with nothing but the plan's choice):

| Model | Parameters | Plan | Result | Host memory per task: reserved / measured peak |
|---|---|---|---|---|
| ResNet18 (224×224) | 11.2 M | fits as is | 3 rounds, 63 s including upload | 1 024 (the 1 GiB floor) / 652 MiB |
| ViT-tiny (224×224) | 5.5 M | fits as is | 3 rounds, 45 s | 1 418 / 807 MiB |
| ConvNeXt-tiny (224×224) | 27.8 M | fits as is | 3 rounds, 168 s | 3 633 / 1 797 MiB |
| SmolLM2-135M (64 tokens, tied head) | 134.5 M trained | 3 stages | 3 rounds, 265 s (about 75 s per round) | 4 095 / 2 666 MiB |

Four defects stood in the way, all fixed:

- **The host-memory limit of a task was wrong.**
  - A whole-model task counted its activations three times. ConvNeXt asked for 7 470 MiB, so no
    4 GiB worker ever claimed it, while the plan said "fits as is".
  - A stage task was sized from the parameters of the whole model and four times its activations.
    That made SmolLM2 ask for 14 987 MiB per stage, and a model split because no machine holds it
    could not be placed at all.
  - The limits now come from measurement: `graph_grad_ram_bytes` and `graph_stage_ram_bytes` in
    `gg-compute/src/catalog.rs`. A stage's limit is its memory estimate, which counts its own
    parameters only, plus the executor.
- **The plan checks host memory.** It reports `task_ram_bytes` and `workers_with_ram`, says so when
  no worker has enough memory, and splits into stages within the smaller of accelerator memory and
  host memory (`gg-control/tests/collective.rs`).
- **Partitioning into stages derived every candidate stage from the whole graph**, so its cost grew
  with the cube of the node count. A plan for SmolLM2 (1 900 nodes) took 108.6 s of server time; it
  now takes 0.4 s. The partition derives each candidate from an index built once and gives the same
  bounds as before (`gg-compute/tests/graph_stages.rs`, `indexed_partition_matches_the_naive_one`).
- **The Python client failed on a rate limit (429)** while uploading the 272 parameters of a staged
  model. Like the Rust client, it now replays on 429 and 503, honours `Retry-After`, and gives up
  after 60 s (`gg-python/tests/test_client.py`).

### Peer-to-peer distribution

Workers exchange immutable bytes, never leases or authority
([ADR-0012](adr/0012-p2p-artifact-transport.md)). The control plane hashes each chunk from its own
bytes, signs the manifest, tracks holders, and issues signed read tickets bound to an artifact and
the reader's key. A holder checks tickets offline; a peer never decides an authorization. Each chunk
is verified before it is written, the full hash is checked again, and any chunk no peer serves
correctly is fetched from the control plane, which remains the durable copy.

Peer traffic is TLS 1.3 only, with raw public keys (RFC 7250) and an ephemeral transport key whose
SHA-256 digest the reader pins; no X.509, no plaintext fallback
([ADR-0013](adr/0013-peer-tls-and-nat.md)). Reachability: observed address returned by the control
plane, IPv6 global addresses preferred, then NAT-PMP, then UPnP-IGD; the control plane dials back
an announced address before advertising it.

| Claim | Evidence |
|---|---|
| Honest peer serves every chunk; lying peer detected on the first chunk and dropped; vanished peer replaced by the control plane; seven forms of forged or stale request refused | `gg-worker/tests/p2p.rs` |
| Wrong pinned digest: handshake fails before any request (0 bytes served) | `gg-worker/tests/p2p.rs` |
| Tracker restricted to workers assigned on the artifact; manifest digests match the bytes served; unreachable or non-global addresses refused (422) and never advertised; `p2p_enabled = false` switches the routes off | `gg-control/tests/p2p.rs` |
| Only the producer may announce its own output (direct hand-off) | `gg-control/tests/p2p.rs` |
| One connection per peer and per transfer: 8 chunks over a relay adding 20 ms each way, 688 → 389 ms; several chunks in flight: 16 chunks, 734 → 221 ms; a lying peer is dropped in both modes | `gg-worker/tests/p2p.rs` |
| Swarm: two simultaneous readers of 16 chunks request 16 instead of 32 from the control plane | `gg-worker/tests/p2p.rs`; `cargo xtask demo --kind collective --p2p` with workers started together: 18 to 20 chunks exchanged between peers, 10 to 12 from the control plane (0 and 30 before) |
| Lying peer in the reference demo | `cargo xtask demo --kind collective --p2p --faults`: corrupted chunks rejected against the manifest, 40/40 tasks accepted, final checkpoint equivalent |
| UPnP-IGD on a real consumer router: mapping created, confirmed in the router's table, deleted and confirmed absent | `gg-worker nat-probe --map 47654 --lease 60`; `gg-worker/tests/upnp.rs` (real SOAP exchange) |
| NAT-PMP against a gateway speaking RFC 6886 | `gg-worker/tests/natpmp.rs` (real UDP exchange, bounded timeout) |

**Lab node** ([ADR-0041](adr/0041-lab-node.md), A1 to A7). A researcher registers
the key of a node running on their own machine (`POST /researcher/nodes`); the node signs in by
challenge, takes a file into its store, declares its hash, size and chunk digests for a project
(`POST /node/artifacts/{hash}`) and serves the chunks itself, announcing its address (dialled back
by the control plane) and renewing the announcement. The server records the declaration, signs the
manifest from the node's digests and keeps no copy: a worker assigned a task over such an input gets
`409` from the control plane and every chunk from the node. The digests are computed over the copy
the node keeps, so the scientist may change the original file afterwards.

A job may name one of the researcher's nodes as `output_holder` (A4). Only workers that serve peers
(`serves_peers` at claim time) are given it; they keep their large outputs, declare their chunk
digests and serve them, and the control plane refuses them as uploads above
`node_relay_max_bytes` (0 by default: the server carries no data of such a run; a private
deployment may allow small ones). The node lists what waits for it (`GET /node/pulls`), fetches each
output from its producer with a ticket issued to the node, checks it, and declares it pulled; only
then is the output no longer pending. An output not pulled within `pending_output_secs` closes its
attempt as inconclusive (not paid, nobody sanctioned) and sends the task again.

Validators read what the control plane does not store through peers (A5): the node's inputs, the
outputs it pulled, with a ticket issued to the validator, each chunk checked against the signed
manifest. Node-held inputs are prefetched the same way. When they cannot be read (the node
offline), the validator decides nothing and releases the verification: a worker is never judged on
an artifact nobody can serve.

A graph run may name a node as `holder_node` (A6): the node draws the initial weights from the seed,
averages each round's accepted gradients and applies the optimizer step with the coordinator's code,
holds every checkpoint, and reports it; the server checks it against the run's shard plan and opens
the next round. The server keeps the description only.

The node is a program of its own, `gg-node`, installed as a user service by a signed installer
(`deploy/node/install.sh`, like the worker's). It finds its servers in a list signed with the release
key (`https://gpugoblin.com/servers.json`) and works only with a server that presents the key the
list names. The Python library only needs its address (`gg.Client(..., node=...)`): datasets and
starting weights go to the node, the node creates and steps the runs, and progress and trained
weights are read from it. Its local API checks the researcher's token with the control plane,
listens on the node's machine by default, and needs TLS to listen on the laboratory's network.

A node belongs to a laboratory, not to one researcher (A7, step 1). Its owner grants other
researchers the use of it. Each of them attaches it to their projects, and it works only for the
attached projects. It cannot be detached while it still steps a run or pulls a job's outputs there.

A node that keeps failing to pull an output (its producer gone, or serving bytes that do not match
what it declared) gives up on it after `give_up_after`
(`POST /node/artifacts/{hash}/unavailable`): the task is sent again at once, to another worker,
with the same consequence as the deadline. It gains the node nothing it could not get by staying
offline.

| Claim | Evidence |
|---|---|
| A job over two node-held inputs is accepted; the assigned worker takes 3 of 3 chunks from the node and 0 from the control plane, which answers `409`; the peer list says the server holds no copy; a revoked node is no longer advertised and a reader then waits for peers and gets `409`, never a chunk from the server | `gg-node/tests/hold.rs` |
| A worker that does not serve peers is not given a job a node holds; one that does has its 128 KiB output refused as an upload (relay limit set to 1 KiB) and serves it; the node pulls 2 of 2 chunks from the worker, holds the exact bytes, and nothing waits any more | `gg-node/tests/pull.rs` |
| An output the node never pulls: past its deadline the task is sent to another worker (generation 2) and no longer offered to the node | `gg-node/tests/pull.rs` |
| A real `gg-verifier` recomputes a task whose two inputs and output only the node holds, reads all three through peers and accepts it; with the node and the worker gone, it decides nothing, releases the verification, and the task stays submitted | `gg-node/tests/pull.rs` |
| The node gives up on an output whose producer vanished after waiting its request timeout for peers (not falling back to a server that will never hold it); the task goes to another worker at once, never back to the same one | `gg-node/tests/pull.rs` |
| Real binaries: `cargo xtask demo --kind matmul --node`: 12/12 tasks accepted in 3.9 s, 36/36 artifacts (24 inputs, 12 outputs) on the node, none in the control plane's object store, 120 chunks from peers and 0 from the control plane. With `--faults` (a worker corrupting its outputs, a peer serving corrupted chunks, an interrupted lease): 12/12 accepted in 47.5 s, no corrupted result paid | `cargo xtask demo --kind matmul --node [--faults]` |
| A training run stepped by the node (3 rounds × 3 tasks, Adam, bf16 gradients, real `gg-verifier`): the node's checkpoint equals a single-process replay bit for bit; the server holds no copy | `gg-node/tests/train.rs` |
| Real binaries: `cargo xtask demo --kind graph --node`: 10 rounds, 40/40 tasks accepted in 4.3 s (3.3 s when the server steps), checkpoint equal to the replay (largest difference 0), 82/82 artifacts on the node. With `--faults`: 18.8 s (17.1 s when the server steps) | `cargo xtask demo --kind graph --node [--faults]` |
| A shared node: another researcher may attach it only once its owner allows him, and only to his own projects; the node holds for attached projects and for no other; withdrawing the grant keeps what was attached; detaching is refused while a job still sends it outputs, then accepted, after which the node no longer holds for that project and no job may name it | `gg-node/tests/share.rs` |
| The installed node (`gg-node run`), through its local API only: datasets in, run created, round tasks by state, trained weights out, bit-exact against a replay; unknown or missing tokens refused, a project the caller is not a member of refused; a server presenting another key than the list names is not worked with | `gg-node/tests/train.rs` |
| A node run starts from supplied weights (fine-tuning): step 0 is the bundle as it is, the result equals a replay from those weights; small gradients the server relayed are read from it | `gg-node/tests/train.rs` |
| The signed list of servers: accepted only fresh, signed by a trusted key in its own namespace, not older than one accepted; `https` only | `gg-node` unit tests; by hand with `ssh-keygen -Y sign` (accepted; altered: refused; older, validly signed: refused) |
| Installer: signature, digests, configuration, key, in a scratch home (`--no-start`) | `deploy/node/install.sh` against a local release signed with a test key |
| The Python library through a real `gg-node`, a real control plane, 3 real workers and a real validator: 8 of the live tests pass (MLP, causal LM, RoPE, YaRN, `transformers` Llama and Mixtral, LSTM, Mamba) | `GG_NODE=http://127.0.0.1:7460 pytest tests/test_live.py` on a local grid (`gg-tools/live.sh`, P2P=1) |
| The local API over TLS (a self-signed certificate, checked by `curl`); plain http refused on that port | by hand |
| Reachability shared by worker and node (`gg_client::reach`): candidates in order (configured, IPv6, gateway mapping, bound, observed), a duplicate dropped, no mapping asked on loopback, the accepted address kept until the server refuses it | `gg-client` unit tests (`reach`) |
| A node nobody can reach: its small artifacts are relayed by the server chunk by chunk (per-chunk ranges and whole downloads), the server keeps nothing, an artifact above the relay limit is not relayed (409), a node that stops answering gives the reader a 503. The relay is off by default (`node_relay_max_bytes = 0`) | `gg-node/tests/relay.rs` |
| The reversed connection (A8 step 3): the node pushes its input to the worker that asked, over TLS pinned to the worker's key; the ticket shown to another reader is refused (403), a chunk not matching the manifest (422), a push without ticket (403); an address the server cannot dial back is skipped; nothing is stored on the server. A worker nobody can reach pushes its output to the node, which then holds it | `gg-node/tests/reverse.rs`, `gg-node/tests/pull.rs` |
| Concurrent stores into the artifact cache never fail on each other's files (before the fix, 2 runs in 3 failed) | `gg-client/tests/cache.rs` |
| A refused download keeps its status and message (`409`): before this fix it surfaced as a bare HTTP error, and a worker whose small input only a peer held failed its attempt instead of turning to peers | `gg-client/tests/download.rs` |
| Only a member of the project declares into it (404 otherwise); a hash already held by another node is refused (409); only the holding node announces it (404); a chunk list that does not match the size is refused (422); a researcher token is not a node session | `gg-node/tests/hold.rs` |

Sharing is opt-in (`[p2p] enabled = false` by default), bounded (connections, byte budget, announce
TTL) and can be switched off on either side. Artifacts in use by an ongoing attempt are pinned in
the cache; every cache hit is re-hashed.

### Model coverage and custom operators

On a corpus of 18 real architectures, 3 were captured before `gg-ops/6` and 16 after, each
checked against PyTorch with perturbed weights (`gg-python/tests/test_corpus.py`,
`python tools/coverage_corpus.py`, `gg.coverage()`):

| Models | Deviation from PyTorch |
|---|---|
| Llama, Qwen2, Mistral, Phi-3, GPT-2, GPT-NeoX, BERT, DistilBERT, ViT, ResNet, ConvNeXt, Whisper (encoder), Mamba, LSTM, Conv1d + GroupNorm | ≤ 4.8e-7 |
| Gemma | 7.5e-6 |
| T5 (computed integer tensors), GNN (`index_add`) | refused, with the reason named |

- `Formula` (an elementwise expression): 12 expressions against their f64 definition (value ≤ 2e-6,
  gradient ≤ 1e-5), bitwise deterministic; on GPU, same loss as CPU and gradient within 1.9e-6
  (`gg-compute/tests/graph_formula.rs`, `gg-compute/tests/gpu.rs`).
- Description functions inline to a description byte-identical, in value and gradient, to the same
  model written flat (`gg-compute/tests/graph_functions.rs`).
- `gpugoblin.formula` captures a Python expression as a single node, with the same refusals as Rust
  (`gg-python/tests/test_formula.py`).
- Hugging Face models as-is: Llama with 4, 2 and 1 key heads (GQA), Qwen2, Mistral and scaled
  Llama 3.1 run in the Rust executor within 1.5e-7 of PyTorch (`gg-python/tests/test_hf_models.py`).

**Operator profile `gg-ops/7` (ADR-0027), 2026-09-24.** Eleven operators added: ConvTranspose2d,
Pad, Resize, Flip, ReduceSum/Max/Min/Prod, CumSum, and IndexSelect/IndexAdd with constant indices.
`Formula` gains fifteen functions: inverse trigonometric and hyperbolic functions, atan2, and
floor/ceil/round/trunc/sign, whose derivative is zero; `round` is half to even on every backend.
Each has its f64 oracle with a finite-difference gradient check, bit-for-bit determinism, named
rejections, FLOP and memory estimators, and a measured GPU tolerance on the RTX 5060 Ti (gradient
≤ 7.6e-6 absolute; `gg-compute/tests/graph_ops7.rs`, `gg-compute/tests/gpu.rs`). The PyTorch driver
now translates transposed convolutions, weight norm, padding, upsampling, reductions,
var/std/logsumexp/log_softmax/norms, flip/roll/repeat, constant-index indexing and about thirty
element-wise functions; 44 constructs match PyTorch within 6.5e-7 (`gg-python/tests/test_ops7.py`). The
corpus goes from 16/18 to 21/22 captured models: T5, a GNN, both HiFi-GAN vocoders and Wav2Vec2 now
compute the same function as PyTorch; a top-2 mixture of experts stays rejected by name (`topk`). A
captured HiFi-GAN and Wav2Vec2 train like PyTorch: loss and gradients within 9.7e-7 of PyTorch's, and
accepted on GPU. Not done: integer tensors computed in the graph (argmax, topk, sort, MoE routing),
data-dependent shapes (text-to-speech models such as VITS and FastSpeech2 stop inside
`torch.export` on them), assignment by index (design in ADR-0027).

**Operator profile `gg-ops/8`: integer tensors (ADR-0029), 2026-09-24.** Every tensor now has a
type (f32/i64) inferred by validation. Seven operators are added: ArgMax, ArgMin (first index on
ties, NaN largest), ArgTopK, ArgSort (stable, computed on the host), Cast, GatherElements,
ScatterElements (add, or assignment of provably distinct positions); Gather accepts tables of rank
3–4. Values of topk/sort/max(dim) are a gather of the indices, which is PyTorch's derivative, so the
format keeps one output per node and descriptions without integers are unchanged. Index ranges are
checked statically, or at loading for data (named error, never a clamp); integers never cross a
stage boundary. Oracles in `gg-compute/tests/graph_ops8.rs`: value ≤ 8.6e-8, gradient ≤ 9.4e-8,
indices exact. GPU (RTX 5060 Ti): identical indices, ties and NaNs included; 0 of 8192 tokens routed
differently. `gpugoblin.batched_experts` switches `transformers` mixtures of experts to static-shape
experts. A Mixtral and a hand-written top-2 mixture of experts train like PyTorch (loss and
gradients within 4.4e-7). Outside, refused by name: data-dependent shapes (nonzero, boolean
masking), one_hot, index_put, integer arithmetic.

**Tied weights.** `torch.export` lifts a tied tensor once per alias, and the driver made each alias
a separate parameter: a language-model head sharing the embedding was trained apart from it
(SmolLM2-135M described with 163.3 M parameters instead of 134.5 M; in the corpus GPT-2, Gemma,
BERT, DistilBERT, T5, Mamba). Tensors that share storage are now one parameter read by every node
that uses it; its gradient sums the uses, across stages too, and a checkpoint reloads into every
alias. Guard: every corpus model has exactly PyTorch's parameter count; a tied Llama matches
PyTorch's loss and gradients, split in stages or not; the staged live test runs tied (85.5%) and
untied (93.3%).

**Batch-independent descriptions.** The first live Mixtral run failed in every task: the driver
recognized a batch-dependent dimension only when it equalled the capture batch, so the `tokens·k`
dimensions of the experts kept the capture's count. The driver now captures twice (batch B and
B+1) and compares: a dimension proportional to the number of examples becomes the free leading
dimension, any other difference is refused by name. Guard: every corpus model and every
ops7/ops8 construct is captured at 2 examples and executed at 3 and 5 against PyTorch; training
fixtures are checked at 3. The guard also found that `Gelu(approximate="tanh")` (Gemma, GPT-2's
`gelu_pytorch_tanh`) was computed with the exact form: fixed, Gemma 1.01e-5 → 3.1e-7.

### Python library

The `gpugoblin` package builds descriptions; no Python function ever crosses the network. It
provides a REST client, `GGT1`/`GGB1` codecs in NumPy, and a PyTorch driver that captures a module
with `torch.export` and translates it into `gg-graph/v1`, naming any operator outside the profile.
`Trainer.plan()` asks the server which levers, stages or tiles fit the announced workers, and
`Trainer.fit()` applies the proposal and reassembles tiles and shards on return. See
[PYTHON.md](https://github.com/GPU-Goblin/gg-python/blob/main/docs/PYTHON.md).

End-to-end tests on a real grid (`gg-python/tests/test_live.py`, debug build, sandboxed workers, one
validator; one worker, or four with P2P for the staged and tiled cases), all 10 passing on
2026-09-24:

| Model | Result |
|---|---|
| 3-class classifier, 384 examples, 25 rounds | loss 1.098 → 0.021, weights reloaded into the module |
| Causal language model written like nanoGPT, 40 rounds | loss 2.99 → 0.0000, accuracy 100% |
| RoPE model, copy at t−2, 60 rounds | accuracy 6.0% → 93.8% |
| `transformers` rotary class in YaRN mode, 60 rounds | accuracy 6.0% → 95.4% |
| `LlamaForCausalLM` (GQA, SwiGLU, Llama 3.1 RoPE), 300 rounds | accuracy 6.2% → 91.2% |
| `nn.LSTM` unrolled with a `gg.formula` activation, 150 rounds | accuracy 6.6% → 99.9% |
| `MambaForCausalLM` as-is, 322 nodes, 60 rounds | accuracy 6.7% → 100% |
| Llama in 4 stages on workers with a third of the memory, 40 rounds | accuracy 5.4% → 93.3% |
| `transformers` Mixtral, 4 experts, top-2 routing, 300 rounds (2 workers) | loss 2.77 → 0.060, accuracy 6.0% → 98.2% |
| 4 MiB matrix on 3 MB workers: 3 tiled matrices, 24 stages, 30 rounds | loss 4.195 → 0.263, matrices reassembled in the module |

Plus 142 unit tests (`python -m pytest -q tests` in gg-python; live and GPU tests are skipped
without `GG_SERVER` and hardware).

### Performance

Measured in release on the RTX 5060 Ti (`crates/gg-compute/examples/matmul_bench.rs`,
`examples/stage_profile.rs`, per-operator profile with `GG_PROFILE=1`):

| Measurement | Result |
|---|---|
| Stage of 4 × 768, batch 8, full pass | 1,664 GFLOP/s (52 on CPU); an earlier 758 GFLOP/s figure came from a debug build |
| Matrix product alone, transformer shapes | 2,673 to 8,225 GFLOP/s |
| Fixed costs of a GPU task in a fresh process | Vulkan init 0.45 s, upload of 162 MB of parameters 0.24 s; kernel compilation about 1.5 s on a small transformer, 1.7 s (ResNet18) to 5.9 s (ConvNeXt-tiny, SmolLM2) on the pilot models, paid by every task (table below) |
| `burn/autotune` | +7% at steady state but about 16 s of tuning per fresh process: not enabled |
| `burn/fusion` | no measured gain: not enabled |

At this scale, per-task fixed costs dominate, not the kernels.

**The GPU grid against a single GPU** (2026-09-24, RTX 5060 Ti; the grid is 1 GPU worker in its
sandbox and 2 CPU validators on the same machine; 32 examples per step, Adam; PyTorch 2.x CUDA
`torch_baseline.py` in `gg-tools`, strict FP32 like the grid, then PyTorch's defaults with TF32):

| Model | PyTorch, one step (FP32 / defaults) | Our executor, in one process (1st call / next ones) | Grid, one round | Of which: GPU task(s) / CPU validation |
|---|---|---|---|---|
| ResNet18, 224 px | 38.9 / 36.2 ms | 2.7 s / 1.03 s | not measured: rejected (see below) | 3.6 s / 18.5 s |
| ViT-tiny, 224 px | 36.9 / 29.6 ms | 5.9 s / 0.80 s | 23 s (5 rounds) | 7.2 s / 15 s |
| ConvNeXt-tiny, 224 px | 224 / 199 ms | 8.4 s / 2.5 s | about 57 s (1 round, from its parts) | 12.7 s / 42.7 s |
| SmolLM2-135M, 64 tokens | 197 / 138 ms | 6.8 s / 1.05 s for 8 sequences | 60 s (3 rounds, 2 stages, 4 tasks) | 25 s / 3 to 17 s per task |

Where a round goes, in order of weight:

1. **CPU recomputation by the validator** decides the round: 15 to 43 s, since a round is aggregated
   only once every result is accepted. A GPU worker computes faster than the CPU validator can
   check it: with a single GPU the grid is 100 to 600 times slower than PyTorch on the same card.
2. **Kernel compilation, paid by every task**: each task runs in a fresh sandboxed executor, which
   compiles its kernels again (ViT: 5.1 s of a 7.2 s task). A compilation cache kept on disk
   brings a fresh process's first call from 6.0 s to 0.84 s (ViT) and from 2.1 s to 1.09 s
   (ResNet18), 900 KB of cache; not enabled: the cache would outlive the task's sandbox, which is
   a design decision (a poisoned cache would reach the next tasks).
3. **The executor itself**, warm: 11 (ConvNeXt) to 27 (ResNet18) times slower than PyTorch with
   cuDNN and cuBLAS; Burn/WGPU in FP32 with generic kernels.
4. **Memory estimates**: SmolLM2 at 32 sequences is estimated at about 30 GB and split into 2
   stages, where PyTorch peaks at 5.4 GB in one piece; the stages add 3 extra tasks per round.

**Accuracy against float64** (ResNet18, 224 px, 32 images, same weights and batch; PyTorch float64
on CPU as the truth): our CPU executor, median relative error per parameter 2.7e-3 and largest
element gap 7.5e-4; our GPU executor 3.9e-3 and 1.0e-3; PyTorch FP32 on CUDA 5.3e-3 and 1.1e-3.
The GPU is as accurate as PyTorch. The validator nevertheless rejected it: the element-wise
tolerance of catalog 1 (1e-4 absolute + 1e-3 relative) was calibrated on two-layer transformers and
was tighter than the float32 rounding of its own CPU reference (7.5e-4 from the truth) once batch
normalization sums over 32 × 112 × 112 values (71 of 76 checks passed; the 5 failures were gaps of
2e-4 to 8.5e-4).

**After ADR-0031** (validation on the GPU, a tolerance by norm, CPU engine checks). Same grid with a
GPU validator and a CPU validator taking only audits and engine checks; audits at 25 % and engine
checks at 10 %, the defaults, so about a third of the tasks also wait for a CPU recomputation:

| Model | One round, GPU validation only | One round with a CPU check | Before (CPU validation) | GPU validator's recomputation |
|---|---|---|---|---|
| ResNet18 | 6.7 s | 24 s | rejected | 1.4 s (CPU: 18.5 s) |
| ViT-tiny | 9.0 s | 23 s | 23 s | 1.06 s (CPU: 14.9 s) |
| ConvNeXt-tiny | 19.3 s | 67 s | about 57 s | 3.2 s (CPU: 47 s) |
| SmolLM2-135M, 2 stages | 37 to 47 s | — | 60 s | 0.8 to 2.9 s per stage task |

All four train to completion (ResNet18 now accepted). What remains, per round without a CPU check:
the worker's task (kernel compilation included) and fixed costs. Against PyTorch on the same card
the grid stays 86 (ConvNeXt) to 240 (ViT) times slower: the executor and the per-task compilation
(items 2 and 3 above) are the next levers. Found on the way: a GPU validator kept its memory pool
at its peak (6.9 GB after SmolLM2) and a worker sharing its card failed, with a panic or non-finite
gradients (refused by the executor, never submitted); the pool is now released after each
computation, which brings it to 5.8 GB, not to zero.

**Executor, first pass** (2026-09-24; one gradient of 32 examples, warm, in one process; RTX 5060
Ti). Four defects of the executor itself, all bit-exact to fix, weighed more than the choice of GPU
library:

- a Dropout of rate 0 drew its mask on the host anyway: 275 of the 300 ms of a ViT-tiny forward
  pass. It is now the identity (multiplying by 1.0 is exact);
- the forward pass ran twice in every unsegmented step (once without the autodiff tape, once with
  it); the last segment is now computed once;
- every parameter and dataset was copied on the host before its transfer (55 ms per 113 MB), and
  every gradient once more on its way back; they now move;
- each task compiled its GPU kernels again: kept between tasks (ADR-0032).

| Model | Vulkan, before | Vulkan, now | LibTorch through Burn (CUDA), now | PyTorch |
|---|---|---|---|---|
| ResNet18 | 1030 ms | 870 ms | 205 ms | 39 ms |
| ViT-tiny | 800 ms | 280 ms | 84 ms | 37 ms |
| ConvNeXt-tiny | 2510 ms | 2360 ms | 1900 ms | 224 ms |
| SmolLM2, 8 sequences | 1050 ms | 820 ms | 550 ms | about 50 ms |

On the grid, with the kernel cache: a ViT-tiny task from 6.7 to 1.1 s and its round from 9.0 to
3.5 s; ResNet18 from 3.1 to 1.4 s, round 5.0 s; ConvNeXt-tiny from 11.9 to 2.8 s per task. Burn's
CUDA backend (CubeCL, the same kernels as Vulkan) gained at most 1.6× and needs NVRTC ≥ 12.8 on an
RTX 50. What remains, measured: the backward pass of a depthwise convolution in Burn (311 ms for one
7×7 layer against 3 ms in PyTorch, about 1.7 s of ConvNeXt's step), host-device transfers (on
LibTorch a 113 MB download takes 117 ms, on Vulkan an upload 160 ms), and the convolution kernels on
Vulkan.

**Why Burn stays far from PyTorch even on LibTorch** (measured the same day). A depthwise 7×7
convolution, 32 images: the input gradient takes 10 ms, the weight gradient 306 ms on LibTorch and
205 ms on Vulkan, because Burn computes the weight gradient of a grouped convolution with a loop over
the groups. Two exact rewrites inside the executor (the weight gradient as one grouped transposed
convolution, then as a sum of shifted products) were measured no faster, the first because it
becomes a convolution with a 56×56 kernel, the second because of the autodiff graph it builds, and
were not kept. More generally Burn computes every backward pass with its own generic formulas, even
on LibTorch, where PyTorch calls dedicated cuDNN and ATen kernels: ResNet18 takes 205 ms through
Burn on LibTorch against 39 ms in PyTorch on the same card. A ConvNeXt fixture now checks the
gradients of depthwise convolutions against PyTorch (`graph_ops7.rs`).

**Native CUDA engine** (ADR-0033, 2026-09-25): the executor's own operators on LibTorch with
PyTorch's autograd (cuDNN and ATen backward kernels), strict float32 (`NVIDIA_TF32_OVERRIDE=0`;
TF32 put ResNet18 7 % away), NVIDIA only, Vulkan kept for other cards.

| Model | Gradient in one process: Vulkan → native CUDA (PyTorch) | Grid round without a CPU check: Vulkan → native CUDA |
|---|---|---|
| ResNet18 | 870 → 130 ms (39 ms) | 5.0 → 3.5 s |
| ViT-tiny | 280 → 72 ms (37 ms) | 3.5 → 2.35 s |
| ConvNeXt-tiny | 2360 → 270 ms (224 ms) | 19.3 → 6.5 s |
| SmolLM2-135M, 2 stages | 820 → 440 ms (≈ 50 ms, 8 sequences) | 37–47 → 23–33 s |

The grid ran a CUDA worker in its full sandbox, a CUDA validator and a CPU validator for audits
and engine checks, with no disputes. SmolLM2's 440 ms are 73 ms of computation (PyTorch's level)
and 370 ms of transfers (parameters in, gradients out); starting a CUDA executor costs about 0.8 s
per task; stage tasks still use Burn's autodiff. Evidence: `NVIDIA_TF32_OVERRIDE=0 cargo test -p
gg-compute --features cuda --test cuda` (13 models against the CPU under the validators' rule, 5
against PyTorch's gradients), `gg-worker doctor` with `--cuda-init` under the full restriction.
Packaging since ADR-0039 (below).

**Executor sessions** (ADR-0034, 2026-09-25): on a GPU engine one sandboxed executor serves the
tasks of one training run instead of a fresh process per task (restricted once; ended at a run
change, after 200 tasks, after any failure). CUDA worker's executor time per task: ViT-tiny 1.2 →
0.33 s (72 ms of computation), ResNet18 1.45 → 0.47 s (130 ms); grid rounds without a CPU check
1.45–2.45 s and 2.5 s. Evidence: `gg-worker/tests/sandbox.rs`
(`an_executor_session_serves_several_tasks_in_one_sandbox`: two tasks in one restricted process,
names outside the session root refused), and the grid. Pinned transfer buffers were measured: a
single transfer of 540 MB takes 77 ms pinned against 285 ms pageable, but without `unsafe` (forbidden
in gg-compute) the extra copy and allocation cancel the gain; not adopted. For SmolLM2 the time now
goes mostly outside the executor: fetching the checkpoint (1 s) and uploading its gradient (up to
2.2 s for 540 MB).

**Gradients in bf16** (ADR-0035, 2026-09-25): tasks send their parameter gradients in bf16 by
default, half the bytes; weights, the server's average and Adam's moments stay f32;
`gradient_precision = "f32"` keeps every bit. Measured first in PyTorch (`gg-tools/bf16grad.py`, 4
simulated workers, 2 seeds): rounding moves a gradient by 0.16 % of its norm, against 21–200 % between
two batches; final loss and accuracy (byte-level transformer, ResNet18 on CIFAR-10) differ no more
than when only the last bit of the f32 gradients is changed. On the grid (CUDA worker and validators,
all local): uploaded gradient per task ResNet18 44.7 → 22.4 MB, SmolLM2 540 → 270 MB (upload 1.83 →
0.95 s, executor 1.9 → 1.5 s), every decision accepted, CPU engine checks included. SmolLM2 rounds
barely move here (17.5–20.2 → 11.3–19.8 s): 8.8 s of a 16.5 s round is the server's own step (read
weights and moments, write 1.6 GB). Evidence: `gg-protocol` `bf16_rounds_to_nearest_even_and_is_typed`,
`gg-compute --test graph_task` (`a_bf16_gradient_is_half_the_bytes_and_verified`: bytes, rounding,
acceptance, wrong type and wrong value rejected), `gg-control --test collective` (every replay
rounds as the grid and still matches bit for bit; an f32 run matches the unrounded replay).

**The server's step** (ADR-0036, 2026-09-25): each trained shard's weights and Adam moments stay
in memory between rounds (`step_cache_bytes`, 4 GiB), used only for the exact weights (content hash)
and step they belong to; only the weights are written before the next round opens, the moments
behind it, recorded once durable (`opt_lag = 1`: at most one step behind the weights). After a
restart the moments are rebuilt exactly from the recorded gradients of the round that produced the
weights. Bundle encoding without copies, hashing beside the fsynced write, parallel reads and a
multi-threaded Adam step (bit for bit the serial formulas). SmolLM2 server step 9.3 → 3.1 s
(read 2.7 → 0.47, compute 0.6 → 0.28, store 6.0 → 2.4), rounds 16.5–20 → 12.1–12.9 s on one machine.
Evidence: `gg-control --test collective` (`a_graph_run_survives_losing_its_kept_state`,
`a_staged_run_survives_losing_its_kept_state`: state dropped every 20 ms, moments rebuilt, final
checkpoint bit for bit the replay's; `a_graph_run_without_kept_state_is_the_same`),
`gg-compute --test graph_task` (`the_parallel_server_step_equals_the_serial_formulas`).

**Validator prefetch** (ADR-0037, 2026-09-25): `POST /validator/upcoming` lists the durable inputs
of tasks being computed or awaiting verification, for the kinds the validator's profile may
verify, and the validator may read them before holding the verification; its prefetcher downloads
them into its cache while the worker computes (one lock per artifact shared with verifications,
which also fetch inputs and outputs at once). SmolLM2: validator download 2.06 → 0.92 s, every
input from the cache; rounds 12.1–12.9 → 11.4–12.7 s on one machine. Evidence: `gg-control --test
e2e` (`a_validator_may_prefetch_the_inputs_of_tasks_being_computed`: nothing while queued,
announced and readable while computed or submitted, not for a profile that may not verify the kind,
gone and unreadable once decided).

**Native stage backward** (ADR-0038, 2026-09-25): on the CUDA engine a stage's backward pass uses
PyTorch's autograd, and parameters and gradients cross to and from the card without Burn's extra
host copies (whole-model tasks too). Two backward passes of a model in two stages: ConvNeXt-tiny
2765 → 316 ms, ResNet18 220 → 169 ms, SmolLM2 1030 → 890 ms (its stages are transfers: 35 ms of
computation, 150 ms up, 250 ms down). On the grid, ConvNeXt-tiny in stages 4.5–7.4 → 2.8–4.2 s per
round, SmolLM2 in stages unchanged (17–20 s); every decision accepted. Evidence:
`cargo test -p gg-compute --features cuda --test cuda`
(`native_stage_backward_passes_the_model_task_rule_against_the_cpu`: transformer, Llama, ConvNeXt,
ResNet18 in 2 and 3 stages against the CPU chain, worst relative norm 1.9e-4). The native engine now
runs the memory levers too (segments, offloading), as stages in one process: a CUDA worker used to
fail the tasks the plan gave it with segments (ConvNeXt-tiny on 1.5 GB cards; now trains, rounds
3.5–4.2 s); `memory_levers_run_natively`.

**Beta rehearsal** (2026-09-25, `gg-tools/rehearse_beta.sh`, DEPLOY.md §15):

- **What ran.** A release built from the public repositories as the workflow builds it and signed
  offline with a throwaway key. The server and validator ran on the production configuration files.
  A CUDA worker and a CPU worker were installed by `install.sh`. The smoke test and two Python
  training runs passed (loss 1.098 → 0.021, 301 results accepted, 301 receipts verified). Backup,
  pruning, and restore into a new database followed; the restored server serves every current
  checkpoint.
- **Two defects found and fixed.** A CUDA worker claimed and failed a matrix product: workers now
  claim only the kinds their engine may produce. The backup's pruning looked in the wrong
  directory.
- **Not rehearsed.** Caddy/HTTPS, the PostgreSQL Unix socket, and the systemd hardening.

**Retention** (ADR-0040, 2026-09-25): a training run's intermediate artifacts (workers' outputs,
micro-batch rows, superseded optimizer moments, weights of the checkpoints not kept) are removed once
their round is settled for good (every task accepted, failed or cancelled, every verification
decided, no dispute) and ten minutes have passed; step 0, every 50th checkpoint, the current one and every
researcher upload stay; while a run goes on, the round that produced its current checkpoint stays
whole. Until now nothing was removed: about 2 GB per SmolLM2 round. Evidence: `gg-control --test
collective` (`retention_removes_only_what_a_graph_run_no_longer_needs`, `…_staged_run_…`: the
policy every 20 ms and the kept state dropped every 20 ms during the run, bit for bit the replay;
then kept checkpoints download, the others say they expired, workers' outputs are gone, no
researcher upload is).

**Installing the CUDA engine** (ADR-0039, 2026-09-25): `install.sh --cuda` installs the signed
`gg-executor-cuda` with the GPU build of the worker (only the executor links LibTorch), downloads the
official LibTorch 2.9.0+cu128 archive from PyTorch and refuses it unless its size and SHA-256 are
those of the signed `libtorch.sha256` (pinned digest cross-checked against the server's S3 ETag),
and configures `engine`, `libtorch_dir` and `device_memory_bytes`. Rehearsed in a temporary home
against a locally signed release: installed, doctor passed (CUDA under the full sandbox, cgroups
required), reused on reinstall, a tampered archive and an unsigned change of the pin refused with
nothing installed; the installed worker trained ResNet18 on a local grid (3 decisions accepted). The
rehearsal found and fixed a defect: the doctor's cgroup probe could not load a CUDA executor. The
CUDA tests pass against the official libraries. Not done: the release workflows have not run on
GitHub.

**Round duration**, `LlamaForCausalLM` in 4 stages, 3 workers at a third of the model's memory,
same loss bit for bit (2.5132; 2.5048 since the generator's domain tag was renamed, see
[ADR-0026](adr/0026-speculative-waves.md)) at every step:

| Change | 4-stage round | Data-parallel round |
|---|---|---|
| Baseline (coordinator polling every 2 s) | 21.7 s | 2.0 s |
| Event wake-ups and speculative waves (1 validator / 3 validators) | 8.9 s / 4.6 s | 0.41 s |
| `TCP_NODELAY` on control plane and peers (download fixed cost 292 → 9 ms) | 1.48 s | 0.156 s |
| Pipelining between micro-batches | 1.27 s | |
| Per-task overheads (inputs 41.3 → 2.6 ms, outputs 22.5 → 10.5 ms, parallel I/O, P2P threshold 1 MiB), P2P on | 1.37 → 0.55 s | |

With a simulated 4 MB/s uplink (about 2 MB per activation), handing activations off before the
durable copy brings a 4-stage round from 29.1 s to 22.7 s (−22%).

**Cost of a lost worker** (Llama in 4 stages, 4 workers, 3 validators, 30 rounds, release build;
one worker killed while it holds a task at round 10): with a fixed 120 s lease, the run takes 162 s
instead of 34 s (the round waits 124 s for the lease to expire). Leases now follow the heartbeat
rhythm the worker announces (four heartbeats, at least 15 s) and the worker keeps its lease for the
whole attempt, downloads and uploads included: the same failure costs 22 s (run 55.9 s, affected
round 23.4 s, two runs). The trained model is identical with and without the failure (loss
2.7839 → 0.5890, accuracy 95.5%).

**Load and full cost of an accepted result** (`cargo test -p gg-control --test load --release --
--ignored`, [BENCHMARKS.md](BENCHMARKS.md)): at 512³, 0.015 s of compute per accepted result, of
which 89% is verification overhead; at 1024³, 0.075 s and 88%; 11 to 61 results/s depending on
size; p95 assignment latency 39 to 51 ms; no work lost. With the 0.25 audit rate, an accepted result
costs about 2.25× its useful compute ([ECONOMICS.md](ECONOMICS.md)).

**Planning.** `gg job plan` estimates duration, cadence and what limits it before submission
([ADR-0017](adr/0017-distribution-plan.md), [ADR-0018](adr/0018-dataflow-scheduler.md)). For a
1.3 B-parameter model on 100 M tokens and 100 homes on fibre (300 Mbit/s up) with a 10 TFLOP/s
executor, activations routed through the coordinator would take 5.1 days; routed peer to peer,
1.7 h. On home fibre the executor sets the pace; on ADSL the link does. These are plans, not runs;
a 125 B-parameter sizing in [MODELS.md](https://github.com/GPU-Goblin/gg-core/blob/main/docs/MODELS.md) was never executed, and nothing has run on a Mac.

### Release, licensing and operations

| Claim | Evidence |
|---|---|
| Signed release: manifest built unsigned on the build machine, signed offline with `gg release sign`; `gg release verify` accepts an intact release and refuses a modified byte, an unlisted key and a revoked key | `cargo xtask release` on the 5 real binaries; [RELEASE.md](RELEASE.md) |
| Dependency and license inventory: no undeclared license, no AGPL/SSPL/BUSL | `cargo xtask licenses` ([DEPENDENCIES_INVENTORY.md](DEPENDENCIES_INVENTORY.md)) |
| Incident procedures that only use existing commands and name what is not tooled | [INCIDENTS.md](INCIDENTS.md) |
| Review guide mapping each invariant to a test | [REVIEW.md](REVIEW.md); **no independent review has taken place** |
| Private-beta deployment: hardened systemd units (separate users, seccomp, `ProtectSystem=strict`), Caddy for HTTPS, production configuration, daily backups with incremental objects and a rehearsed restore (same server key and row counts, receipt verified), health check with optional webhook | `deploy/server/`, [DEPLOY.md](DEPLOY.md), [RUNBOOK.md](RUNBOOK.md); rehearsed locally with release binaries under the unit hardening (user transient units, no root here); shellcheck and Caddy not run (not installed) |
| Signed worker distribution: `gg release sign` also writes `SHA256SUMS` and an OpenSSH signature over it, so a contributor verifies with `ssh-keygen -Y verify` and `sha256sum -c` without running a downloaded binary; `install.sh` pins the key, installs without root and requires `gg-worker doctor` | `gg-protocol` test against the real ssh-keygen in both directions; install rehearsed: good release (CPU, GPU) installed, tampered binary, foreign key and edited checksum list refused with nothing installed |
| Withdrawal of a validator (sessions revoked, held verifications back to the queue) and of an artifact (runs and jobs using it cancelled with reservations released, bytes deleted, hash never served or accepted again) | `gg-control/tests/federated.rs`, `e2e.rs`, `collective.rs`; [INCIDENTS.md](INCIDENTS.md) |
| Researcher sign-in by e-mail link (single use, 15 min, confirmation click, accepted domains, per-address limit, account pending admission), ADR-0028 | `gg-control/tests/portal.rs`, including the real SMTP path against a relay on this machine; not tried with a real mail provider |
| Behind the proxy: client address from `X-Forwarded-For` of trusted proxies only; per-client quotas | `gg-control/tests/quota.rs`, `client_ip` unit tests |
| Workers below `min_worker_version` get no task (426) and stop, saying they must be updated; SIGTERM stops every binary gracefully | `e2e.rs`, `gg-worker/tests/upgrade.rs`, `gg-control/tests/signals.rs` |
| Contributor accounts (ADR-0030): anyone signs up with an e-mail code, no admission; a machine attaches itself with the account's enrollment code through its own key-proven session and works at once; suspending an account stops all its machines | `gg-control/tests/contributors.rs`, `gg-worker/tests/enroll.rs`; `install.sh --enroll` |

Global check: `cargo xtask check` (fmt, clippy `-D warnings`, CPU tests against a real PostgreSQL,
contract and Wasm artifact) passes.

## Simulated or limited

- **ORCID**: the real Sandbox has never been exercised; development uses a simulated provider
  (`dev_mode` only) and admin-issued API tokens. E-mail goes out through a real provider since
  2026-10-08 (Brevo SMTP relay, STARTTLS, password from the unit's environment file): one sign-in
  message from the deployed grid was received. The contributor code e-mail is not yet tried there
  (sign-up opened on 2026-10-11). On the deployed grid the former HTML portal
  (`/portal…`) redirects to the site's account page; the JSON APIs are unchanged.
- **Validation** is performed by authorized, administered validators under distinct operators. It
  is not trustless: collusion between operators is not addressed, and there are no automatic
  financial sanctions. The audit is a sample: a dishonest worker passes a given task with
  probability `1 − audit_rate`. Verification by other contributors (ADR-0042) works for what the
  server stores and for jobs a lab node holds, off by default. A colluding pair escapes the spot
  checks with probability about `1 − spot_check_rate` per task once past probation; what a
  suspended pair had accepted earlier is not recomputed automatically; a task waits as long as
  fewer than two (three in probation) contributor accounts able to run it are online; a node that accepts wrong
  results is not caught (it harms only its own run). The replica is paid as much as the first
  computation (the owner's choice).
- **Fault injection** is local worker configuration (`[dev_faults]`), refused in public mode, never
  remotely activatable. Likewise `sandbox = "none"` requires `dev_unsafe_no_isolation = true` and is
  refused in public mode.
- **cgroups** are applied through `systemd-run --user --scope` when the user session delegates
  `cpu memory pids`; otherwise the worker falls back to rlimits with a warning (`cgroups = "require"`
  makes it mandatory).
- **Single control plane**: one server instance, local object store as the only durable copy (no
  S3-compatible adapter), quotas and metrics held in memory per process (reset on restart, not
  shared between instances, no latency histograms). The outbox is drained by an internal loop.
- **Peer to peer**: direct transfers only between reachable workers. No hole punching between two
  restrictive NATs, no PCP, no relay, no discovery outside the tracker. NAT-PMP has never been
  confirmed on a real router; inbound IPv6 was not verifiable from the test network. Workers that
  cannot reach each other fall back to the control plane with no visible failure.
- **Lab node (ADR-0041)**: milestones A1 to A6. The node steps data-parallel graph runs over
  whole datasets or per-task rows it cuts itself, staged runs it cut into stages and tiled runs
  it tiled (MLP runs are still stepped by the server); it keeps no state in memory between rounds and
  applies no retention to its disk (the rows it cuts and the activations of staged rounds stay
  there too). When one side of a transfer cannot be reached, the other is pushed to
  (the reversed connection, A8 step 3). When neither can, nothing passes: hole punching (step 4)
  is not done, and the server relay is off by default. A validator reads from an unreachable
  holder only if it is given `push_listen`. The reversed connection has not been tried across a
  real NAT; the tests run on loopback. The cutting (stages, tiles, micro-batch rows) is still the server's; the owner decided
  it moves to the node. Its release is not yet part of the published release workflows (the
  public repositories' CI files are hand-written); the TLS path of its API is checked by hand only. The demonstration runs the node inside `xtask`,
  not as its own process. A worker whose output
  the node never pulls is not paid for that attempt, even when the node was the one offline.
  Reachability is the worker's first rung only: an address the control plane can dial back (A8).
- **Resident weights (P11 D1) are not implemented**: each stage task downloads its stage's shards;
  the cache only helps for shards that did not change (frozen ones). Keeping trained weights resident
  would move the optimizer to the worker, which changes verification.
- **Host memory**: outside staged execution, a worker holds the whole model in host memory
  (offloading only frees accelerator memory).
- **Precision and engines**: f32 only (no bf16), which doubles memory relative to half precision;
  Vulkan through WGPU is the only GPU engine (no Metal). GPU tests are run manually on identified
  hardware; there is no GPU CI runner.
- **Collective MLP** (`mlp.grad.v1`): synchronous rounds, no compression, no asynchronous rounds,
  no model averaging. A run whose project runs out of budget stays `running` with its round open
  until the project is credited or the run is canceled (`gg job collective-cancel`).
- **Operators outside the profile**: T5 (computed integer tensors), GNNs using `index_add`,
  bidirectional LSTM, interleaved RoPE (GPT-J), multimodal RoPE (Qwen2-VL), rotary modules with
  several layer types (Gemma 3), ALiBi, T5 relative biases, non-causal masks, incremental inference.
  "Proportional" RoPE (zero frequencies) is accepted by the description but has never been captured
  from a real model. No ONNX import, no NumPy tracer for non-ML computation.
- **Planning** does not model contributor failure; the cost of a failure during a staged run has
  not been measured.
- **Settlement**: contract tested on a simulated chain only; no chain chosen, no audit, no
  deployment.
- **Memory estimate** counts tensors the executor holds, not the process's resident memory; the
  engine's internal buffers come on top, so contributors should declare memory with a margin.

## Open risks

[RISKS.md](RISKS.md) lists 14 residual risks, 4 of them blocking for a public deployment (R1
collusion between validation operators, R2 server key compromise with no tooled rotation, R3
non-reproducible binaries, R4 no independent review), and the decisions that belong to a human.
Decisions already taken are recorded in [DECISIONS.md](DECISIONS.md).

Smaller technical risks: claims are served by a server loop polling every 500 ms (with event
wake-ups on top), measured to about 194 claims/s with 60 workers and no double assignment (beyond
that, consider `LISTEN/NOTIFY`); a claim scans up to 16 candidates per transaction, so a project
without budget at the head of the queue costs one read per claim; the matmul tolerance covers the
GPU deviation at K = 2048 for inputs in [-1, 1) and must be re-evaluated for other distributions.

## Last full verification

2026-09-25, on the development machine described above:

| Command | Result |
|---|---|
| `cargo xtask check` | fmt, clippy `-D warnings`, 436 tests passed, 0 failed; 2 ignored on purpose (the load test, run explicitly, and one timing measurement) |
| `cargo xtask demo` with no option, `--faults`, `--kind matmul`, `--kind collective`, `--kind collective --faults`, `--kind collective --p2p --faults`, `--kind matmul --federated --faults`, `--kind graph`, `--kind graph --faults`, `--kind matmul --p2p --faults`, `--kind matmul --node`, `--kind matmul --node --faults`, `--kind graph --node`, `--kind graph --node --faults`, `--kind matmul --gpu --workers 1` | all 15 OK |
| `GG_BIN=<path to gg> python -m pytest -q tests`, in gg-python | 262 passed; the live tests are skipped without a grid |
| the live Python tests through a real `gg-node` (local grid, 3 workers, 1 validator, P2P) | 8 passed, 3 skipped |
| after the cutting by the node (rows, stages, tiles): `cargo xtask check`; `cargo xtask demo --kind graph --node`, `--kind graph --node --faults`, `--kind matmul --node`, `--kind graph`, `--p2p`, `--kind collective --p2p --faults`; the Python tests | 401 tests passed; the 6 demos OK; Python 262 passed, 11 skipped (live tests without a grid). The live Python tests through a real node were not run again |
| after the reversed connection (A8 step 3) and the relay turned off by default: `cargo xtask check`, then `cargo xtask demo --kind matmul --node`, `--kind graph --node`, `--kind matmul --node --faults`, `--kind graph --node --faults`, `--p2p`, `--kind matmul --p2p --faults` | 396 tests passed; the 6 demos OK. The 9 other demos and the Python tests were not run again after this change |

Found while verifying (2026-09-25): a request cancelled while it opened a database transaction (its
client gone) could hand its connection back to the pool still inside that transaction. sqlx 0.9 does
not roll back a `begin()` dropped between sending `BEGIN` and counting it. Every later request on
that connection then wrote into a transaction nobody committed, while answering success. It showed
as a declared output that vanished (1 test run in 4). The pool now asks the server, when a
connection comes back, whether it is inside a transaction block, and rolls it back
(`gg-storage/tests/pool.rs` reproduces the leak and failed without the check).

Also found: two demonstrations had been failing unnoticed.

- `--kind graph` has failed since gradients travel in bf16 (ADR-0035): `gg job collective-check` replayed with f32 gradients. The replay now rounds each gradient as the workers do, and the published checkpoint matches it bit for bit (largest difference 0).
- `--kind matmul --p2p --faults` never caught its lying peer: every job had its own inputs, so nobody needed what the liar held. In this mode, the jobs now share one matrix.

The GPU and CUDA test suites and the live Python tests were not run again this time; their last results
(2026-09-24) are below.

| Command (2026-09-24) | Result |
|---|---|
| `cargo test -p gg-compute --features gpu --test gpu` (RTX 5060 Ti) | 19 passed |
| `NVIDIA_TF32_OVERRIDE=0 LIBTORCH_USE_PYTORCH=1 cargo test -p gg-compute --features cuda --test cuda` (RTX 5060 Ti, LibTorch 2.9.0 cu128) | 3 passed |
| the 10 live tests against a real server, validators and 1 to 4 workers (release build) | 10 passed (table above) |
| a worker killed while it holds a task, Llama in 4 stages | the run completes; 22 s lost; same trained model |

Defects found and fixed while verifying (2026-09-23 and 24): a perceptron training killed while it
wrote a checkpoint could resume from a mix of two checkpoints; the collective fault demonstrations
could finish before their second fault; a worker's lease covered only execution, not downloads and
uploads; descriptions of mixtures of experts kept the capture's batch size in a reshape;
`Gelu(approximate="tanh")` was computed with the exact form; the host-memory limits of model tasks
kept ConvNeXt and a staged SmolLM2 off every 4 GiB worker; the plan took 108.6 s for a 1 900-node
model; the Python client failed on the rate limit instead of waiting (pilot models, above); on
the GPU, a convolution whose kernel is shorter than its stride (a ResNet's 1×1 stride-2 shortcut)
had a wrong input gradient and a transposed convolution in the same case a wrong result, in Burn's
WGPU kernels (100 % error; never seen by the tests, where such a convolution came first); a
staged run of 32 sequences was refused, a stage's output size counting every activation twice;
capturing a module left it in evaluation mode (GPU grid, above).

## Reproducing

```bash
cargo xtask db up                    # PostgreSQL 17 in Docker
cargo xtask check                    # fmt, clippy, tests against real PostgreSQL
cargo xtask demo                     # reference scenario; also --kind matmul|collective|graph,
                                     # --faults, --gpu, --p2p, --federated
# in gg-core:
cargo test -p gg-compute --features gpu --test gpu     # identified GPU required
# in gg-python, with the gg binary built here (cargo build -p gg-cli):
GG_BIN=../gg-server/target/debug/gg python -m pytest -q tests   # GG_SERVER=... enables test_live.py
```

Demo reports are written to `var/demo/run-<timestamp>/report.json`. GPU results and P2P
measurements are detailed in [BENCHMARKS.md](BENCHMARKS.md).
