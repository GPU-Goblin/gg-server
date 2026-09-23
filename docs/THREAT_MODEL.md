# Threat model

Status of each countermeasure: **implemented+tested**, **implemented**, **planned (phase)**.
Anything that is not "implemented+tested" is absent from a security point of view.

## Actors

- Malicious researcher: tries to run arbitrary code, exhaust someone else's budget, read results
  from another project, or get a job outside the catalog accepted.
- Malicious worker: submits false results, replays submissions, claims several payments, submits
  late on an expired attempt, reports false capabilities, sends booby-trapped files.
- Compromised validator: accepts false results (limited by administrative diversity — P6).
- Hostile network: replay, interception, redirection to internal services (SSRF).
- Compromised driver/GPU on the contributor's machine: beyond the reach of process isolation.

## Threats and countermeasures

| Threat | Countermeasure | Status |
|---|---|---|
| Execution of code submitted by a researcher | Compiled catalog; typed parameters; no binary/Wasm/shell/container accepted | implemented+tested (P1) |
| Custom operator used as a gateway for code (`Formula`, description functions, ADR-0025) | Closed, bounded grammar (1024 bytes, 128 terms, 32 levels, fifteen named functions, no loops or variables); closed functions, without recursion, inlined by the server under the 4096-node bound; the executor only evaluates a tree of Burn operations | implemented+tested (P11) |
| False activation passed on before verification (speculative waves, ADR-0026) | The coordinator records the attempt behind each output passed on; a single one that does not end up accepted poisons the round, all of whose jobs are canceled and which is reissued; nothing is aggregated until every task of every wave has been accepted; the cheater is not paid and their signature identifies them | implemented+tested (P11) |
| Producer declaring false chunk digests for an activation handed off before its durable copy (ADR-0026) | The signed manifest only certifies transfer integrity: the reader re-verifies the full digest, the one in the signed submission, and rejects any other bytes; no verification or payment before the durable copy, which is SHA-256 checked; a copy missing at the deadline poisons the round | implemented+tested (P11) |
| Peer announced before it holds the artifact (swarm, ADR-0012) | It only serves chunks already verified against the signed manifest and answers 404 for the others; the reader re-verifies every chunk and the full digest; an assigned peer that does not deliver costs at most `swarm_patience_ms`, after which the control plane serves | implemented+tested (P5) |
| Worker announcing itself as holder of an artifact it has no right to know about | Announcement allowed for an input of an active attempt, or for an output this worker uploaded itself (`uploaded_by`); every chunk is still verified against the manifest the server signs from its durable copy | implemented+tested (P11) |
| Shell command built from a task | Executor launched with typed arguments, cleaned environment, no interpolation | implemented (P1) |
| False result or reported metrics | Reference recomputation by a validator, tolerances fixed before execution | implemented+tested (P1) |
| Replay of an already accepted submission | Uniqueness of `(task_id, generation)` + idempotency keys bound to the actor | implemented+tested (P1) |
| Late message on an expired attempt | Monotonic generation, server time, lease checked within a transaction | implemented+tested (P1) |
| Double payment | `UNIQUE(reward_key)`; single balanced entry | implemented+tested (P1) |
| Double budget reservation | Budget row lock + `CHECK` | implemented+tested (P1) |
| Signature replayed on another network/domain | Tag `gpu-goblin/v1\n<network>\n<domain>\n` | implemented+tested (P1) |
| Worker key self-proclaimed as a validator | Distinct roles in the database, explicit admin authorization | implemented+tested (P1) |
| Anyone signs up as a contributor (open sign-up, ADR-0030) | Every result recomputed by a validator before acceptance; audit by a second operator; a machine is attached only through its own key-proven session; suspension of an account stops all its machines at once | implemented+tested |
| A stranger's machine sees research data | Stated in the data rule accepted by researchers; no confidentiality against contributors; restricting sensitive projects to trusted machines is **not** implemented | documented (open) |
| Many accounts from one person (Sybil) | Without rewards of value, only the attacker's own compute is spent; admission or stake needed before any reward of value | documented |
| Cross-project access | Every route checks project membership; guessed ids are not enough | implemented+tested (P1) |
| Booby-trapped tensor file (size, shape, NaN) | Bounded `GGT1` parser, shapes/dtypes checked, non-finite values rejected | implemented+tested (P1) |
| Absolute paths / traversal / links | Output names controlled by the system, paths built on the agent side | implemented (P1) |
| SSRF through a job URL | No URL supplied by a job; artifacts by hash only | implemented (P1) |
| Executor accessing the network / files outside its bundle | bubblewrap (namespaces, no network, read-only system) + Landlock + seccomp + rlimits; real probe in `worker doctor` (ADR-0009) | implemented+tested (P3) |
| Executor spawning a process or exceeding RAM | seccomp denies `execve`/`fork`; `RLIMIT_AS` on CPU; no cgroups | implemented (P3), cgroups planned |
| Secrets leaking into logs/errors | Generic external errors, `tracing` without secrets | implemented (P1) |
| Silent auto-update | No such mechanism; manual signed updates | planned (P3) |
| Eavesdropping on traffic between peers | Mandatory TLS 1.3 with a raw public key (RFC 7250) pinned to the fingerprint published by the control plane; no plaintext fallback (ADR-0013) | implemented+tested (P5) |
| Impersonation of an announced holder | The handshake fails against any key other than the one registered by the server: the requested artifact is not even revealed | implemented+tested (P5) |
| Reuse of the identity key for transport | Separate, ephemeral transport key, regenerated at each start; the protocol identity only signs protocol messages | implemented (P5) |
| Peer supplying false bytes | Each chunk verified against the server-signed manifest before being written; peer dropped for the transfer; full hash re-verified; HTTP fallback (ADR-0012) | implemented+tested (P5) |
| Peer exfiltrating local files | Only cache artifacts are served, addressed by hash; no file name supplied by the caller; sharing disabled by default | implemented+tested (P5) |
| Unauthorized reader requesting bytes from a peer | Ticket signed by the server, bound to the artifact and the reader's key, expiring; request signed with that key | implemented+tested (P5) |
| Server connection callback used as a probe | The callback follows the same address policy as the announcement (private addresses rejected in public mode), is time-bounded and neither writes nor reads anything | implemented+tested (P5) |
| Tracker steering a worker toward an internal address (SSRF) | Address policy on both sides: 0.0.0.0, multicast, link-local and port 0 always rejected; loopback/private only if explicitly allowed, forbidden in public mode | implemented+tested (P5) |
| Peer overwhelming a contributor | Connection limit, outbound byte budget, announcement expiry, sharing can be disabled | implemented+tested (P5) |
| Discovery of contributors' addresses | An announcement is only visible to workers authorized on that artifact; never to researchers or the public; announcement optional and expiring | documented (P5) |
| Isolated complacent or failing validator | Unpredictable audit by a distinct operator; a disagreement does not pay and opens a dispute settled by an operator (ADR-0014) | implemented+tested (P6) |
| Engine defect shared by GPU workers and a GPU validator (ADR-0031) | A validator recomputing on the GPU is admitted for model tasks only; a sample of what it decides (`engine_check_rate`, drawn from the server secret after the decision) is recomputed again on the CPU, only by a CPU validator, whose decision must state the CPU profile; nothing is aggregated or paid before it, and a disagreement opens a dispute | implemented+tested |
| Bias within the tolerance of model tasks (ADR-0031) | Per tensor and per task, a dishonest worker can shift a gradient by up to 2 % of its norm and be accepted (`‖a − b‖ ≤ 2e-2·‖b‖`); bounded, diluted by the aggregation of the round, every task recomputed; the tolerance is four times the measured honest gap, not more | documented (residual) |
| A task leaving a malicious compiled kernel for the next ones (kernel cache, ADR-0032) | The cache is the only thing a task leaves behind: per worker, never shared between machines, emptied on every executor update and over its budget; later tasks run in the same sandbox and their results are still recomputed (and sampled on the CPU); a contributor can disable it (`kernel_cache_max_bytes = 0`); entries are not verified (nothing to compare a compiled kernel with) | documented (residual) |
| Native CUDA engine: a large third-party library (LibTorch, cuDNN) inside the sandbox (ADR-0033) | Read-only at `/opt/libtorch`, same sandbox as every task (no network, Landlock, seccomp); the only extra write is the process's own `/proc/<pid>/task` (thread names, needed by the driver); results recomputed, a sample on the CPU; LibTorch pinned (2.9.0), its archive not yet verified by the installer | documented (partial: packaging to do) |
| A tampered LibTorch reaching CUDA contributors (ADR-0039) | Not redistributed: downloaded over https from download.pytorch.org only, refused unless its size and SHA-256 match `libtorch.sha256`, which the release key signs (digest cross-checked against PyTorch's S3 ETag when pinned); unpacked under its digest; runs only inside the executor's sandbox; a changed pin without a new signature is refused | implemented, install rehearsed (tampered archive and unsigned pin refused) |
| A validator reading inputs of tasks it does not hold yet (prefetch, ADR-0037) | Only inputs (never a worker's output) of tasks being computed or awaiting verification, of kinds its profile may verify, durably stored; unreadable once the task is decided; validators are administered operators who see these inputs when verifying | implemented+tested (`e2e.rs`) |
| Server state kept in memory between rounds going stale or exhausting memory (ADR-0036) | An entry is used only for the exact weights (content hash) and optimizer step it belongs to; memory bounded by `step_cache_bytes`, least recently stored evicted; moments referenced only once durable, at most one step behind the weights, rebuilt exactly from recorded gradients after a restart | implemented+tested (`collective.rs`: state dropped during runs, final checkpoint bit for bit) |
| A worker hiding a deviation in the rounding of a bf16 gradient (ADR-0035) | The validator compares with its own f32 recomputation under the unchanged norm rule; the rounding takes about a tenth of the margin, leaving the rest as it was; a gradient of the wrong type or size is rejected before any recomputation; weights and optimizer state stay f32 on the server | implemented+tested (`graph_task.rs`, `collective.rs`) |
| An executor compromised by a task's inputs persisting across tasks (executor sessions, ADR-0034) | GPU engines only; a session serves one training run and ends at a run change, after 200 tasks, after any failure, timeout or lost lease; restricted once before reading any task (same sandbox); tasks of another project start a new session; results still recomputed, a sample on the CPU; `persistent_executor = false` restores a process per task | documented (residual, same run only) |
| Multiplying keys to simulate diversity | Quorum counts operators: two keys from the same operator never verify the same submission | implemented+tested (P6) |
| Worker predicting or choosing its checks | Draw from a server secret after the result is recorded; no worker response reveals it; the worker does not choose its validators | implemented+tested (P6) |
| Fabricated dispute settlement | An operator can only decide on an outcome that a validator signed; a single, audited settlement | implemented+tested (P6) |
| Claiming a reward twice | Claims recorded under the global obligation identifier: the same obligation committed in two batches is paid only once (ADR-0015) | implemented+tested (P7) |
| Replaying a proof on another contract, network or chain | These fields are in the leaf hash and re-checked before any proof | implemented+tested (P7) |
| Payment diverted by a relayer | Funds go to the beneficiary recorded in the leaf, never to the transaction sender | implemented+tested (P7) |
| Root replaced after publication | Republishing a batch identifier is rejected; the observed root is compared with the committed root | implemented+tested (P7) |
| Underfunded batch | Solvency of all open batches checked at publication | implemented+tested (P7) |
| Settlement considered final on a mere transaction identifier | Separate state machine: only an observation followed by declared finality marks a batch as settled | implemented+tested (P7) |
| Collusion between operators | **Unresolved**: cross-verification fails if two operators collude; neither reputation nor the number of keys changes that (ADR-0014) | stated limitation |
| Volume: excessive decompression/allocations | No archive accepted; catalog allocation bounds | implemented (P1) |
| API DoS | Body limits, timeouts; per-actor quotas; a worker or validator's requests covered by its own live lease are bounded by the lease (declared inputs and outputs, lease duration) rather than counted | partial (P1: limits), P3 quotas |

## What the system does not guarantee

- The physical identity of a GPU or the time actually consumed (a receipt attributes a statement
  to a key).
- Data confidentiality with respect to the owner of the computing machine.
- Network anonymity of a contributor who enables peer-to-peer sharing: their `ip:port` is visible
  to the other workers authorized on the same artifact (ADR-0012). Sharing is optional and disabled
  by default.
- The correctness of a computation from the mere existence of a hash or a blockchain entry.
- That a committed root proves a computation: it proves that a list was committed. Acceptance
  remains off chain, signed by validators, and referenced by the receipt.
- Any authorization to issue or market an asset: contract tests replace neither a security audit,
  nor governance, nor a legal review.
- "Trustless" validation: validators are administered and authorized by the control plane
  operator. The guarantee is "several identified administrative entities have recomputed".
- "Trustless" operation: P1 validation depends on an administered validator.
