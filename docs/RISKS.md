# Residual risks and decisions before public release

Status as of 2026-09-17. This document is the exit criterion of P8: it lists what remains true
after everything that has been implemented and measured, and the decisions that belong to a human.

**The system is not ready for public deployment.** A green `cargo xtask check` does not make a
system production-ready; the points below are the reason, not a stylistic precaution.

## Residual risks

Severity: ⛔ blocks a public release, ⚠️ must be addressed, ℹ️ accepted and stated.

| # | Risk | Severity | What already exists | What is missing |
|---|---|---|---|---|
| R1 | Collusion between validation operators: cross-verification fails if two operators collude | ⛔ | Quorum counted in operators, unpredictable audits, recorded disputes (ADR-0014) | Real administrative diversity (distinct organizations, not keys), an operator admission policy, consequences of a detected collusion |
| R2 | Server key compromise: it signs assignments, receipts, manifests and tickets | ⛔ | Key stored with mode 0600, receipts verifiable offline | Online rotation, a signature chain between the old and new key, a rehearsed procedure (docs/INCIDENTS.md states it is not tooled) |
| R3 | Non-reproducible binaries: a third party cannot verify that the bytes match the code | ⛔ | Signed manifest, offline verification, revocation; **key moved off the build machine** (D3) | Reproducible builds or attestations from several builders |
| R4 | No independent review | ⛔ | Review guide, invariants linked to tests (docs/REVIEW.md) | A review actually conducted by a third party |
| R5 | A single control plane: an outage, seizure or operating error stops everything | ⚠️ | Tested backup/restore, append-only registry | Redundancy, a failover procedure, operation by several people |
| R6 | Real ORCID never tested | ⚠️ | Full flow implemented, simulator in development mode only | Sandbox then production credentials, and a measurement of the real flow |
| R7 | A single durable copy: the server's local store | ⚠️ | Content-addressed artifacts, backup | Object storage adapter, off-site copy |
| R8 | Quotas and metrics held in memory per process | ⚠️ | Token bucket per actor, Prometheus metrics | Shared state if there are several instances |
| R9 | Cost: an accepted result costs about 2× its computation | ℹ️ | Measured (docs/ECONOMICS.md); audit rate set to 0.25 and changeable at runtime (D2) | Reassess against real measurements of cheating |
| R10 | A receipt attests a statement attributed to a key, not GPU time or a physical identity | ℹ️ | Written in SPEC and THREAT_MODEL | Nothing: it is a design limit, not to be presented otherwise |
| R11 | No confidentiality with respect to the contributor: they see the data they compute on | ℹ️ | Stated, **rule published and accepted when a project is created** (D4) | Nothing within this scope; sensitive data must not go there |
| R12 | Partial peer-to-peer reachability: no hole punching, inbound IPv6 unverified | ℹ️ | UPnP measured, IPv6 preferred, systematic HTTP fallback | QUIC decision (ADR-0013), based on real bandwidth measurements |
| R13 | No chain chosen for settlement; contract not audited | ℹ️ | Contract tested on a simulated chain, no funds, no deployment | Choice of chain, security audit, governance, legal review |
| R14 | Development mode exists (ORCID simulator, injected faults, isolation that can be disabled) | ℹ️ | Rejected in public mode, never enableable remotely | Operational check that `public = true` everywhere |

## Decisions that belong to a human

Four were taken on 2026-09-17 and are recorded in docs/DECISIONS.md: a public release is the goal
once the blockers are resolved (D1), audit rate at 0.25 and reconfigurable (D2), release key on an
offline machine (D3), synthetic and public data only (D4). The following remain open:

1. ~~**Whether to open up**~~ — decided (D1): a public release is the goal; blockers R1 to R4 must
   be addressed first.
2. **Who operates the control plane**, with which people, which on-call rotation and which
   liability in case of an incident.
3. **Who can be a validation operator**, how their administrative diversity is established, and
   what happens when collusion is detected.
4. ~~**Where the release key lives**~~ — decided (D3): an offline machine. Remaining: who holds it
   and who can revoke.
5. ~~**Which data may enter**~~ — decided (D4): synthetic and public only.
6. ~~**The audit rate**~~ — decided (D2): 0.25 initially, changeable at runtime.
7. **Whether there will ever be a token**, on which chain, with which audit, which governance and
   which legal review — and with explicit authorization, since nothing is deployed today.
8. **The legal scope**: liability for computation run on third-party machines, personal data,
   contributors' jurisdictions.

## What is measured and holds

To avoid rereading the whole repository: `cargo xtask check` passes (fmt, clippy `-D warnings`,
CPU tests against a real PostgreSQL, CosmWasm contract and Wasm artifact). The reference
demonstrations pass, including with injected faults. Load and cost figures are in
docs/ECONOMICS.md, GPU and peer-to-peer measurements in docs/BENCHMARKS.md, and the detailed status
in docs/STATUS.md.
