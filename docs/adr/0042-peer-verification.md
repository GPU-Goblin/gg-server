# ADR-0042 — Verification by other contributors: replicated computation, and its risks

Status: **accepted** (2026-09-26: the owner confirmed the spot-check rate, probation, automatic
suspension pending a human, comparisons checked by contributors, and the replica paid like the
first computation). Proposed 2026-09-25. Decided with the owner (decision 1, option B): results are
verified by other contributors' machines, not by validators the server runs, and the risks are to be
prevented.

## Context

A result is accepted only after someone recomputes it (CLAUDE.md invariant). Today that is an
administered validator: a known operator's machine, which may be the server.

- The cost of verification lands on whoever runs the validator.
- Validators are paid nothing (`verification_cost` goes to a system account).

Option B makes verification a task like any other, computed on contributors' GPUs and paid to them.
The protocol already has a `replica` attempt role, never used until now.

Once anonymous machines vouch for results, the threats change. The ones that matter:

- **T1. Lazy replica.** It copies or guesses instead of recomputing.
- **T2. Colluding pair.** One party controls both computations and returns the same wrong result
  (a Sybil attack when the party opens many accounts).
- **T3. Scientist cheating on the comparison.** Rejecting to avoid paying.
- **T4. Engine defects.** A defect shared by the two machines makes both agree on a wrong result.
- **T5. Honest disagreement.** Floating-point differences between engines.

## Decision

**Every task is computed twice**, by a primary and a replica.

- The two must be different workers of **different contributor accounts**, chosen by the server, not
  by the workers. The same scheduling serves both roles.
- A worker cannot tell whether it computes a primary or a replica.
  - The assignment carries no role.
  - Both are given the same inputs, and neither sees the other's outputs before its own are declared.
- **Commitment before comparison (against T1).** Each declares the digests of its outputs to the
  server before any output of the other is readable. A replica that copies has nothing to copy. One
  that declares garbage loses the comparison.
- **The comparison is deterministic and replayable (against T3).**
  - The lab node, which pulls both outputs anyway (ADR-0041), compares them under the task's
    tolerance (the norm rule of ADR-0031) and signs the result with the measured differences.
  - Either worker may contest it. The server then fetches the two outputs and runs the same
    comparison. A node found to have misreported is recorded and suspended pending a human.
  - The comparison is cheap (one pass over two tensors), and the server runs it only on a
    contestation or a sample.
- **Disagreement (T5, and a first alarm for T1 and T2).**
  - A third computation is assigned, by a third account.
  - The majority within tolerance is accepted, and the odd one out is rejected (not paid).
  - With no majority, a dispute opens for a human, as today.
- **Against collusion (T2).**
  1. **Distinct accounts, drawn at random.** Pairing comes from a server-secret draw over the eligible
     workers of other accounts: an attacker with *k* of *N* active accounts is paired with itself
     about *k/N* of the time, and cannot choose.
  2. **Spot checks.** A sample (`spot_check_rate`, 5 % by default) gets a third computation by a
     third account, whatever the first two said.
     - A pair that agreed while the third disagrees is treated as collusion: both accounts are
       suspended.
     - Everything they had accepted in the run is recomputed.
     - A dispute opens for a human.
  3. **Probation.** A new account's first tasks (`probation_tasks`, 20) are all spot-checked. Its
     results count only as a primary until it has passed.
  4. **Diversity.** Pairing prefers different networks (IPv4 /24, IPv6 /48) and different engines,
     and never pairs two workers of one account.
  5. **Admission.** During the beta, contributor accounts are admitted by the operator
     (`contributor_signup = false` until the mail relay and the policy are settled). This bounds
     *k*, which is the real barrier to a Sybil attack.
- **Engine defects (T4).** Pairing different engines replaces ADR-0031's engine checks: a CPU, a
  Vulkan and a CUDA worker disagreeing on a model points to a defect, not a cheat. The existing CPU
  reference stays the arbiter in a dispute.
- **Payment.**
  - The primary is paid `reward.primary`, and the replica `reward.replica` (the former
    `verification_cost`), both reserved from the scientist's budget at scheduling.
  - A third computation is paid like a replica, from the budget.
  - Nothing is paid until the comparison, or its majority, has accepted: the invariant stands.
  - Receipts name both attempts.
- **Administered validators stay optional.** An operator may still run one as a trusted auditor on a
  sample, and a dispute may still call on it. The grid no longer needs one to run.

## What changes in the invariants (to be written into CLAUDE.md)

- **"No acceptance rests on the metrics the executor declares: the validator
  recomputes"**: *reworded, not weakened*. The recomputation is an independent replica computed by
  another account, chosen by the server. A declared metric is still never accepted.
- **"A disagreement between operators opens a dispute settled by a human, never an automatic
  sanction"**: *exception*.
  - A replica outvoted by a two-to-one majority within tolerance is not paid for that task.
    Suspending an account for collusion (spot check) is a measure pending a human, who confirms or
    lifts it.
  - Without a majority, it is a dispute, as today.

## Residual risks, stated

- An attacker holding a large share of active accounts can still slip wrong results past pairing
  and spot checks, at a rate this ADR quantifies (*k/N* × (1 − spot_check_rate)). What bounds *k*
  is admission. An open public release keeps RISKS.md R1 as a blocker.
- Twice the computation per task, paid by the scientist. This replaces the validator's
  recomputation: the cost was already there, it is now paid to the contributors who carry it.
- A scientist who suspects results can always recompute a sample on their own GPU.

## Plan

This is step 3 of ADR-0041.

1. The replica role and the server-secret pairing with account exclusion.
   *Done (2026-09-25)*, behind `replica_verification` (off by default). The replica is a task of
   its own in a twin job (the task state machine and its generations stay one attempt at a time),
   created when the first computation is assigned so both run at once; the assignment carries
   `primary` for both. Tests: `gg-control/tests/replicas.rs`.
2. Output commitment, and the node's signed comparison.
   *Done (2026-09-25, 2026-09-26)*: `ComparisonV1` (domain `comparison`), the comparison rule in
   `gg_compute::replica`. The server compares what it stores. A lab node compares the computations
   of the jobs it holds (`GET /node/comparisons`, `POST /node/comparisons`, signed with its key); an
   agreement it signs is taken as it is (it pays for what it accepts). **Owner's decision of
   2026-09-26: the server reads none of a node's outputs.** A difference the node reports counts
   once a comparer finds the same: a `replica.compare.v1` task, computed by a contributor of an
   account foreign to the task, drawn by the server, which reads both outputs through peers and
   writes a verdict (a validator could check it exactly, by comparing again). When the comparer finds
   otherwise, a second comparer settles it: the side two agree on counts, and whoever misreported,
   the node or the first comparer's account, is suspended pending a human. The jobs of a suspended
   node are compared by one comparer. Tests: `gg-node/tests/replicas.rs`.
3. Disagreement, the third computation and majority.
   *Done (2026-09-25)*: a third computation by a third account; the majority accepted, the odd one
   rejected and not paid; no majority opens a dispute per computation, settled by an operator on
   any outcome (the receipt references the comparisons).
4. Spot checks, probation and suspension.
   *Done (2026-09-26)*: `spot_check_rate` (5 %) of the families, drawn with the server's secret,
   and every task an account computes while it has fewer than `probation_tasks` (20) accepted
   computations, get a third computation from the start, so it runs alongside and costs no
   latency; nothing is decided before all three are compared. A pair that agreed, contradicted by
   the check, has both accounts suspended pending a human, and a dispute per computation. Tests:
   `gg-control/tests/replicas.rs`. *Not done*: recomputing what a suspended pair had accepted
   earlier in the run; the human who settles the disputes decides it.
   The beta configuration turns it on (`deploy/server/server.toml`, 2026-09-26). A computation drawn
   for a worker does not wait for it while that worker is busy, or once its account computed
   another computation of the task: the reservation only keeps a colluder from choosing.
   Retention (ADR-0040) prunes the twin jobs with their round.
5. Payment of replicas, and receipts.
   *Done (2026-09-26)*: the owner keeps the replica paid like the first computation (the scientist
   pays twice); a comparer is paid its own small price. No validator's `verification_cost` is
   reserved or charged for a replicated computation. A receipt references the signed comparison,
   which names both computations.

Tested:

- honest pairs are accepted;
- a lazy replica is caught;
- a colluding pair is caught by a spot check and suspended;
- a node that misreports a comparison is caught on contestation;
- engine disagreements are resolved by majority.
