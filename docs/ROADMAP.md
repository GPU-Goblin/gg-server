# Roadmap

Last updated: 2026-09-23.

This document summarizes what has been delivered, phase by phase, and what comes next. The evidence
behind each delivered item (tests, commands, measured numbers) is in [STATUS.md](STATUS.md); the
reasoning behind each design choice is in the [ADRs](adr).

## Delivered

| Phase | What it delivered | Key references |
|---|---|---|
| **P0** Scoping and feasibility | Specification, threat model, first ADRs, Burn CPU trial, GPU check | [SPEC.md](SPEC.md), [THREAT_MODEL.md](THREAT_MODEL.md), [DEPENDENCIES.md](DEPENDENCIES.md), ADR-0001 to ADR-0007 |
| **P1** First vertical slice | A real `tensor.matmul.v1` task goes through submission, lease, execution, validation, signed receipt and a single ledger write, with PostgreSQL, a CPU worker, a validator and internal credits | [ARCHITECTURE.md](ARCHITECTURE.md), [PROTOCOL.md](https://github.com/GPU-Goblin/gg-core/blob/main/docs/PROTOCOL.md) |
| **P2** Independent training runs | Deterministic synthetic datasets, `mlp.train.v1` (SGD/Adam, checkpoints and resumption), verification by replay, grid sweeps and comparison | [ADR-0008](adr/0008-mlp-train-protocol.md) |
| **P3** Controlled pilot | ORCID authentication and admission, executor sandbox (bubblewrap, Landlock, seccomp, cgroups) with `worker doctor`, quotas, resumable downloads, backup and restore, GPU execution on identified hardware | [ADR-0009](adr/0009-executor-sandbox.md), [ADR-0010](adr/0010-orcid-authentication.md) |
| **P4** Collective training | `mlp.grad.v1`; server-side coordinator with disjoint slices, single weighted aggregation, one optimizer step per round, round reissue, late results discarded | [ADR-0011](adr/0011-collective-training.md) |
| **P5** Peer-to-peer data | Server-signed chunk manifests, signed read tickets, per-chunk verification before write, HTTP fallback; TLS 1.3 with pinned raw public keys, NAT-PMP, UPnP-IGD and IPv6 reachability | [ADR-0012](adr/0012-p2p-artifact-transport.md), [ADR-0013](adr/0013-peer-tls-and-nat.md) |
| **P6** Federated validation | Quorum counted in operators, unpredictable audits drawn after the result is recorded, disputes settled by a human on a signed outcome | [ADR-0014](adr/0014-federated-validation.md), [VALIDATION.md](VALIDATION.md) |
| **P7** Reward commitments (off-chain) | Single Merkle commitment definition shared by backend and CosmWasm contract, settlement state machine; tested on a simulated chain only, no funds, no deployment | [ADR-0015](adr/0015-reward-settlement.md) |
| **P8** Readiness for opening | Load and full cost of an accepted result measured, signed releases, license inventory, incident procedures, economic assessment, review guide, residual risk report (conclusion: not ready) | [RISKS.md](RISKS.md), [ECONOMICS.md](ECONOMICS.md), [RELEASE.md](RELEASE.md), [INCIDENTS.md](INCIDENTS.md), [REVIEW.md](REVIEW.md) |
| **P9** Models described as graphs | `gg-graph/v1` and a published operator profile, named refusals, cost estimates, distribution plan, execution on CPU and GPU, `graph.grad.v1`, the Python package and PyTorch driver; then the causal mask, positional encodings and scaled RoPE | [ADR-0016](adr/0016-computation-graphs.md), [ADR-0017](adr/0017-distribution-plan.md), [ADR-0020](adr/0020-causal-mask.md), [ADR-0021](adr/0021-positional-encoding.md), [ADR-0022](adr/0022-scaled-rope.md), [MODELS.md](https://github.com/GPU-Goblin/gg-core/blob/main/docs/MODELS.md), [PYTHON.md](https://github.com/GPU-Goblin/gg-python/blob/main/docs/PYTHON.md) |
| **P10** Memory levers | Activation recomputation, parameter offloading and frozen parameters executed in the executor, with measured peaks; dataflow scheduling and peer-to-peer activations decided on measurement | [ADR-0018](adr/0018-dataflow-scheduler.md), [ADR-0019](adr/0019-when-a-stage-does-not-fit.md) |
| **P11** Models split across machines, driven from PyTorch | See below | [ADR-0023](adr/0023-hugging-face-models.md) to [ADR-0026](adr/0026-speculative-waves.md) |

### P11 in detail

P11 started from an audit: each worker still ran the whole model on a share of the data, stages
were planned but not executed, tiling a matrix was manual, and a real `transformers` Llama could not
be captured. The order chosen follows the measurements: stages across machines first (a few
activation exchanges per micro-batch), tiling a matrix on one device next, and splitting a matrix
across peers only as a last resort (one exchange per layer, roughly twenty times more traffic than
stages). A weight's gradient never leaves the machine holding that weight.

- **C1, capabilities.** Workers announce host memory, device memory, profile and engine on every
  claim; each task carries its estimated memory and minimum profile; the server only offers what
  fits, and the executor refuses a task larger than the declared memory. Declaring less memory than
  the hardware has simulates a small device, which makes the rest developable on one machine.
- **C2, sharded checkpoints.** Up to 40 shards of up to 1 GiB, derived deterministically from the
  description; aggregation shard by shard. The checkpoint limit went from 1 GiB to 40 GiB.
- **C3, `transformers` models as-is** (`gg-ops/5`). Llama, Qwen2, Mistral and Llama 3.1 compute
  the same function as PyTorch; a Llama trains on the grid.
- **C4, complete `Trainer`.** `trainer.plan()`, memory levers chosen from announced capabilities,
  refusal before upload if nothing fits, row-sharded datasets and server-provided micro-batches.
- **D2, staged execution** (`graph.stage.v1`): forward or backward pass of one stage, verified like
  any task, coordinated in waves, with checkpoint shards aligned on stages.
- **D3, a matrix larger than a machine**: automatic tiling pass, applied by the server on request.
- **D4, the driver in one call**: the plan proposes levers, stages, or tiles then stages, and
  `Trainer.fit()` applies the proposal.
- **D5, speed (measured)**: per-operator profile and per-task fixed costs quantified; `autotune`
  and `fusion` tried and not kept. The remaining gain is outside the kernels.
- **D6, cost of stages (measured)**: on a real Llama, ÷ 3 memory per task, × 10.8 round duration,
  × 8 verified tasks, × 5.3 bytes stored (before D7).
- **D7, speculative waves and direct hand-off**: event wake-ups, next wave opened on submitted
  outputs, round reissued if a forwarded output is rejected, producers serving their activations
  peer to peer, hand-off before the durable copy, `TCP_NODELAY`, micro-batch pipelining, reduced
  per-task overheads, swarm exchange, connection reuse and several chunks in flight. A 4-stage round
  went from 21.7 s to 1.27 s, then 0.55 s with P2P on, with the same loss bit for bit.
- **E, custom operators** (`gg-ops/6`): `Formula` and description functions; 16 of 18 corpus
  architectures captured, each checked against PyTorch.

### Publication readiness

Done: PolyForm Noncommercial 1.0.0 license with an explanatory [LICENSING.md](../LICENSING.md),
usage-oriented README, `CONTRIBUTING.md`, `SECURITY.md`, `CHANGELOG.md`, GitHub CI (fmt, clippy,
tests on a real PostgreSQL, reference scenario, Python tests), issue and pull request templates, no
secrets in tracked files or history.

## What comes next

This is no longer a sequence of phases. Items are grouped by kind; within each group they are listed
roughly by measured value.

### Decisions that belong to a human

From [RISKS.md](RISKS.md); decisions already taken are in [DECISIONS.md](DECISIONS.md) (public
opening as the goal once blockers are lifted, audit rate 0.25, release key on an offline machine,
synthetic and public data only). Still open:

1. Who operates the control plane: people, on-call, responsibility in an incident.
2. Who can be a validation operator, how administrative diversity is established, and what happens
   when collusion is found.
3. Who physically holds the release key and who can revoke it.
4. Whether there will ever be a token: which chain, which audit, which governance, which legal
   review. Nothing is deployed, and nothing will be without explicit authorization.
5. Legal scope: responsibility for computation run on third-party machines, personal data,
   contributors' jurisdictions.
6. The persistent executor: keeping a GPU executor warm between tasks would remove most per-task
   fixed costs (Vulkan init 0.45 s, kernel compilation about 1.5 s), but today every task runs in a
   fresh sandboxed process. A long-lived executor changes the isolation model and must be decided,
   not slipped in.

### Blockers before any public deployment

- **R1** Collusion between validation operators: real administrative diversity, admission policy.
- **R2** Server key compromise: online rotation with a signature chain between old and new keys,
  and a rehearsed procedure.
- **R3** Reproducible builds, or attestations from several independent builders.
- **R4** An independent review, actually carried out by a third party ([REVIEW.md](REVIEW.md) is
  ready for it).

### Speed

- Per-task GPU fixed costs: warm executor (decision above), persistent cache of compiled kernels,
  larger micro-batches per task.
- Fixed costs of verification.
- Half precision (bf16): today f32 only, which doubles memory.

### Scale

- **Resident weights (P11 D1)**: keep a stage's weights on the machine that holds them, with
  affinity and a backup copy. Not done: each stage task still downloads its shards. Keeping trained
  weights resident moves the optimizer to the worker, which changes verification; measurements so
  far say it is not the first cost to attack.
- Complete the P10 exit criterion: a model that fits on no machine of the bench trains to the end
  with at least one provoked contributor failure, and the checkpoint stays comparable to a
  reference. Measure the cost of a failure during a staged run instead of assuming it is zero, and
  model failure in `gg job plan`.
- Re-measure the full cost of an accepted result with stages after D7.

### Model coverage

- Out of profile today: T5 (computed integer tensors), GNNs using `index_add`, bidirectional LSTM
  ([ADR-0025](adr/0025-custom-operators.md)).
- Positional variants not supported: interleaved RoPE (GPT-J), multimodal RoPE (Qwen2-VL), rotary
  modules with several layer types (Gemma 3), ALiBi, T5 relative biases; non-causal masks;
  incremental inference.
- ONNX import restricted to the profile; a NumPy tracer for non-ML computation.
- A Metal engine (nothing has run on a Mac).

### Network

- NAT hole punching between two restrictive NATs (UDP/QUIC transport and a rendezvous), PCP, a
  relay. The QUIC decision should rest on a measurement of real bandwidth
  ([ADR-0013](adr/0013-peer-tls-and-nat.md)).
- Confirm NAT-PMP on a real router and inbound IPv6 from an outside network.

### Operations

- Real ORCID Sandbox, then production, as soon as client credentials exist (no code change
  expected; the flow must be measured).
- Shared quotas and persistent metrics for several server instances; redundancy and failover for
  the control plane.
- An S3-compatible object storage adapter and an off-site durable copy.
- Tooling for the incident procedures [INCIDENTS.md](INCIDENTS.md) names as untooled: validator
  deauthorization, server key rotation, artifact withdrawal.
- A GPU CI runner; GPU tests are manual today.
- Job tracking in the portal (the CLI and API already cover it).
