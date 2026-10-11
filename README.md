# GPU Goblin

**Train a model or run a large computation by spreading the work across volunteer machines, when
you cannot afford a computing center.** Written in Rust.

A researcher describes a model — in PyTorch or in JSON — and submits it. Volunteer contributors
run units of work on their CPU or GPU. A validator recomputes results before anything is accepted.
Every accepted contribution produces a signed receipt and exactly one ledger entry in test credits.

**No researcher code ever runs on a contributor's machine.** What crosses the network is a model
description and tensors. That is what makes the promise tenable for the person lending a machine,
and what makes the result verifiable by recomputation.

```python
import gpugoblin as gg
from torch import nn

model = nn.Sequential(nn.Linear(6, 24), nn.GELU(), nn.Linear(24, 3))
trainer = gg.Trainer(client, project, model, "cross_entropy", example_input=x[:2])
run = trainer.fit(x, y, rounds=25, batch_per_round=128, tasks_per_round=4, lr=0.05)
trainer.wait(run["run_id"])
trainer.load_checkpoint_into(model, run["run_id"])   # the trained weights come back
```

Measured against a real server: loss from 1.098 to 0.021 in 25 rounds, every result verified by a
validator. See [gg-python](https://github.com/GPU-Goblin/gg-python).

**How long, with how many machines?** To estimate a computation's duration before submitting it,
`gg job plan` uses the model's description and the planned number of contributors. For a
1.3-billion-parameter transformer and 100 million tokens, the documented scenario estimates 10.8 h
with 100 machines, 300 Mbit/s of upload and an executor at 300 GFLOP/s per machine. This figure is
an estimate based on these assumptions, not a measured duration. Details and variants:
[docs/adr/0017-distribution-plan.md](docs/adr/0017-distribution-plan.md).

A model larger than any single machine is split into stages that run on different workers, and a
matrix larger than any single machine is tiled. See
[docs/adr/0024-staged-execution.md](docs/adr/0024-staged-execution.md).

**This is research software, running as a public beta at grid.gpugoblin.com**; no monetary value
is promised for credits. What actually works, what does not, and the risks not yet addressed:
[docs/STATUS.md](docs/STATUS.md) and [docs/RISKS.md](docs/RISKS.md).

## Repositories

| Repository | Contents |
|---|---|
| **gg-server** (this one) | Control plane (`gg-server`), validator (`gg-verifier`), the `gg` command line, ledger, reward contract, reference demonstrations, and the design documents of the whole project |
| [gg-core](https://github.com/GPU-Goblin/gg-core) | Shared crates: domain, signed protocol, approved computations and model descriptions, control-plane client |
| [gg-worker](https://github.com/GPU-Goblin/gg-worker) | The agent contributors install to share their GPU, with its sandboxed executor |
| [gg-python](https://github.com/GPU-Goblin/gg-python) | Python package and PyTorch driver for researchers |
| [gg-console](https://github.com/GPU-Goblin/gg-console) | Operations console (planned) |
| [gg-web](https://github.com/GPU-Goblin/gg-web) | Public website (planned) |

## Getting started (Linux x86_64, CPU only)

Prerequisites: Rust 1.96 (pinned by `rust-toolchain.toml`), Docker (for PostgreSQL 17), no external
keys. The demonstrations also need a checkout of [gg-worker](https://github.com/GPU-Goblin/gg-worker)
next to this one (or `GG_WORKER_DIR` pointing to it), and bubblewrap (`bwrap`) for its sandbox.

```bash
git clone https://github.com/GPU-Goblin/gg-server
git clone https://github.com/GPU-Goblin/gg-worker
cd gg-server
cp .env.example .env
cargo xtask db up              # starts gpu-goblin-postgres on 127.0.0.1:5434
cargo xtask check              # fmt, clippy, unit tests + PostgreSQL integration tests
cargo xtask demo               # server + 3 CPU workers + validator, a grid of 12 MLP trainings, receipts, comparison
cargo xtask demo --faults      # same, with an interrupted worker, a reissue and corrupted results
cargo xtask demo --kind matmul # 12 matrix products
cargo xtask demo --kind collective [--faults]          # collective training: 10 rounds × 4 gradients aggregated by the server
cargo xtask demo --kind collective --p2p [--faults]    # artifacts exchanged between workers, verified chunk by chunk (with --faults, a peer lies)
cargo xtask demo --kind matmul --federated [--faults]  # two validation operators, audits, disputes (with --faults, a lenient validator)
cargo xtask demo --kind graph [--faults]               # a model described as a graph, trained, and compared with a single-process replay
cargo xtask demo --gpu --workers 1                     # workers on the GPU (WGPU/Vulkan, explicit hardware)
```

Each demonstration writes its logs and a `report.json` of pass/fail checks to
`var/demo/run-<timestamp>/`.

## Manual use (after `cargo build --workspace`)

```bash
target/debug/gg-server run                      # server (GG_* variables or deploy/server.example.toml)
target/debug/gg-server bootstrap-admin          # admin token, shown only once
gg admin researcher-create --name Ada --approve # researcher token
gg admin project-create --name p --kind tensor.matmul.v1 --kind mlp.train.v1 --kind mlp.grad.v1 --owner <researcher_id> --ack-data-policy
gg admin project-grant <project_id> --amount 100
gg data gen-matmul --m 256 --k 256 --n 256 --out-a a.ggt --out-b b.ggt
gg job submit --project <project_id> --input a=a.ggt --input b=b.ggt [--dry-run]
gg data gen-mlp --out-dir data/                 # synthetic dataset (train_x/train_y/eval_x/eval_y)
gg job submit --project <id> --kind mlp.train.v1 --input train_x=data/train_x.ggt --input train_y=data/train_y.ggt \
   --input eval_x=data/eval_x.ggt --input eval_y=data/eval_y.ggt --hidden 64 --optimizer adam --lr 0.02 --steps 300
gg job sweep --project <id> --input ... --hidden 32,64 --lr 0.02,0.05 --seeds 3   # 12 independent trainings
gg job compare <job_id> [--out comparison.json]                                   # evaluation loss per configuration
gg job collective --project <id> --input train_x=... --input train_y=... --hidden 32 --batch 256 --tasks 4 --rounds 10
gg job collective-status <run_id> ; gg job collective-cancel <run_id> --yes
gg job collective-check <run_id> --input train_x=... --input train_y=...          # single-process recomputation, measured gap
gg admin worker-authorize <worker_id>          # a contributor's worker (see gg-worker) waits for this
gg admin researcher-list                        # who signed in (ORCID or e-mail link), pending admission
gg admin researcher-admission <id> --state approved --note "..."
gg admin validator-authorize <validator_id> --revoke --yes   # withdraw a validator; its work goes to another
gg admin artifact-withdraw <hash> --reason "..." --yes       # delete an artifact; what uses it is cancelled
gg-verifier init && gg admin validator-add --key <pubkey> --operator op-a && gg-verifier run
gg admin operator-create --name op-b            # separate administrative entity (quorum and audits)
gg admin dispute-list ; gg admin dispute-resolve <id> --outcome rejected --reason "..." --yes
gg worker payout --key worker.key --address <address>          # the contributor declares who gets paid
gg admin batch-create --batch-id b1 --chain-id gg-localnet-1 --contract <addr> [--dry-run]
gg admin batch-export b1 --out batch.json ; gg admin batch-settle b1 --event submitted --tx-hash <hash>
gg job check-model model.json --batch 128        # validate a graph offline (examples in gg-core/examples/models)
gg job ops                                       # the operators this build accepts (gg-ops/8)
gg job eval-model model.json --checkpoint weights.ggb --input ids=ids.ggt --out logits.ggt   # evaluate offline
gg job template transformer --layers 24 --dim 2048 --heads 16 --out t1b.json   # skeleton for sizing
gg job plan t1b.json --examples 97656 --batch 8 --contributors 100 --local-steps 50   # feasible? how long?
cargo xtask release                              # build and write an UNSIGNED manifest (server and worker binaries)
gg release sign release.unsigned.json --key release.key --out release.json   # on an offline machine
gg release verify var/release/release.json --key <key> --network gg-dev-local
gg admin audit-rate --set 0.25 --reason "..."    # share of results re-verified, adjustable at runtime
cargo xtask licenses --out docs/DEPENDENCIES_INVENTORY.md
cargo test -p gg-control --test load --release -- --ignored --nocapture   # load and full cost
gg job status <job_id> ; gg job results <task_id> --out out/
gg project receipts <project_id> --out receipts.json && gg receipt verify receipts.json --server-key <hex> --network gg-dev-local
```

`gg` reads `GG_SERVER_URL` / `GG_TOKEN`, or the configuration saved by `gg auth login`.

Researcher portal: `http://127.0.0.1:8080/portal` (ORCID sign-in; in `dev_mode`, a clearly labeled
ORCID simulator replaces the provider; see the `[orcid]` section of `deploy/server.example.toml`).

## Layout

| Crate | Role |
|---|---|
| `gg-storage` | PostgreSQL (SQLx, migrations) and SHA-256-addressed object storage |
| `gg-ledger` | Budgets, reservations, balanced entries, idempotent rewards |
| `gg-merkle` | Reward batch commitments: canonical leaves, Merkle tree and proofs |
| `gg-control` | Axum `/v1` API, admission, scheduler, leases, outbox, receipts (binary `gg-server`) |
| `gg-verifier` | Administered validator: reference recomputation and signed decisions (`gg-verifier`) |
| `gg-cli` | The `gg` CLI: auth, project, job, worker, receipt, ledger, admin, data |
| `xtask` | Automation: database, checks, demonstrations, release |
| `contracts/gg-rewards` | CosmWasm reward distribution contract, tested on a simulated chain (no real funds) |

The domain, protocol, computations and client come from [gg-core](https://github.com/GPU-Goblin/gg-core)
at a pinned tag.

## Documentation

| To | Read |
|---|---|
| Describe a model and find out whether it fits | [gg-core/docs/MODELS.md](https://github.com/GPU-Goblin/gg-core/blob/main/docs/MODELS.md) |
| Start from PyTorch | [gg-python/docs/PYTHON.md](https://github.com/GPU-Goblin/gg-python/blob/main/docs/PYTHON.md) |
| Know what actually works | [docs/STATUS.md](docs/STATUS.md) |
| What the system promises and does not promise | [docs/SPEC.md](docs/SPEC.md), [docs/THREAT_MODEL.md](docs/THREAT_MODEL.md) |
| How it is built | [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), [gg-core/docs/PROTOCOL.md](https://github.com/GPU-Goblin/gg-core/blob/main/docs/PROTOCOL.md), [docs/adr/](docs/adr/) |
| Check it yourself | [docs/REVIEW.md](docs/REVIEW.md), [docs/VALIDATION.md](docs/VALIDATION.md), [docs/BENCHMARKS.md](docs/BENCHMARKS.md) |
| Deploy a grid on a dedicated server | [docs/DEPLOY.md](docs/DEPLOY.md), [docs/RUNBOOK.md](docs/RUNBOOK.md) |
| Operate, release, respond to an incident | [docs/OPERATIONS.md](docs/OPERATIONS.md), [docs/RELEASE.md](docs/RELEASE.md), [docs/INCIDENTS.md](docs/INCIDENTS.md) |
| What remains to decide or unblock | [docs/RISKS.md](docs/RISKS.md), [docs/DECISIONS.md](docs/DECISIONS.md), [docs/ROADMAP.md](docs/ROADMAP.md) |
| Contribute | [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md) |

## License

[PolyForm Noncommercial 1.0.0](LICENSE): free for research and any noncommercial use,
source-available, not open source in the OSI sense. See [LICENSING.md](LICENSING.md).
