# Independent review guide

This document is for a third party who wants to verify what the platform actually does. It does
not attest to any review: **no independent review has taken place to date**.

## Reproduce in one session

```bash
cp .env.example .env
cargo xtask db up        # local PostgreSQL 17 (docker), 127.0.0.1:5434
cargo xtask check        # fmt, clippy -D warnings, tests (CPU + real PostgreSQL + CosmWasm contract)
cargo xtask demo                                   # 12 independent training runs
cargo xtask demo --kind collective --p2p --faults  # collective training, peers, injected faults
cargo xtask demo --kind matmul --federated --faults # federated validation, disputes
```

A GPU test requires `--features gpu` and identified hardware; an ignored test is not a success.

## Where to start

1. **What is promised**: docs/SPEC.md (user journeys), docs/STATUS.md (what is measured and what is
   not). Comparing the two is the most useful review.
2. **What is threatened**: docs/THREAT_MODEL.md, in particular the section "What the system does
   not guarantee".
3. **The decisions**: docs/adr/0001 to 0015, each with its consequences and limits.

## Invariants and where they are checked

| Invariant | Checked by |
|---|---|
| No work without an approved project, a catalog type and a reserved budget | `gg-control/tests/e2e.rs` (blocking budget, admission) |
| No acceptance based on reported metrics: the validator recomputes | `gg-compute/tests/perturbations.rs`, `mlp.rs`, `grad.rs` |
| A closed attempt is never reactivated by a late message | `e2e.rs` (expired lease → 409), `collective.rs` (canceled round) |
| One reward = one entry, even after a replay | `gg-ledger/tests/ledger.rs`, `e2e.rs` |
| Credits never reserved twice, balanced append-only entries | `gg-ledger/tests/ledger.rs`, `gg-storage/tests/schema.rs` |
| An artifact is referenced only after storage and a SHA-256 check | `gg-storage` (object store), `gg-worker/tests/p2p.rs` |
| Nothing is paid unless every verification has accepted | `gg-control/tests/federated.rs` |
| A disagreement neither pays nor penalizes: an operator decides | `federated.rs` (dispute) |
| A byte from a peer is written only after verification | `gg-worker/tests/p2p.rs` (lying peer) |
| The executor has no network and no access outside its bundle | `gg-worker/tests/sandbox.rs`, `gg-worker doctor` |
| An obligation is paid only once, even from two batches | `contracts/gg-rewards/tests/distribution.rs` |
| Distributed binaries are signed and verifiable | `gg-protocol/src/release.rs`, `gg release verify` |

## Questions a reviewer should ask

- Does the validator really recompute, or does it compare reported metrics? (Read
  `gg_compute::verify_task` and the perturbation tests.)
- What happens if the result's numeric profile is not admitted? (Expected: `inconclusive`, never
  paid — see `profiles::admitted_for` and docs/VALIDATION.md.)
- What exactly does a receipt prove? (Expected: a statement attributed to a key, not GPU time or a
  physical identity.)
- What does a batch root prove? (Expected: that a list was committed, nothing more.)
- Which guarantees fail if two validation operators collude? (Expected: all those that rely on
  cross-verification — this is stated in ADR-0014.)
- Can development mode be enabled remotely? (Expected: no; `dev_mode`, `dev_faults` and
  `dev_unsafe_no_isolation` are local and rejected in public mode.)

## What cannot be verified as things stand

- The real ORCID flow (no Sandbox credentials).
- Inbound IPv6 reachability (no machine outside the development network).
- The NAT-PMP client against a real router (the available gateway does not answer it; UPnP-IGD, on
  the other hand, is measured on a real router).
- Reproducibility of the published binaries (see docs/RELEASE.md).
