# ADR-0014 — Federated validation: operators, unpredictable audits, disputes

Status: accepted and implemented (2026-09-17), phase P6.

## Context

Up to P5, a single administered validator recomputed every accepted task. One key decided, so a
single failure (compromise, bug, complacency) was enough to get a false result paid. The project
brief (§11) asks for the following in this phase: several identified validators; unpredictable audits
whose schedule is never disclosed to workers; a result committed **before** the checks are revealed;
no way for a worker to choose its validators; a distinction between genuine administrative diversity
and a mere multiplicity of keys; and a divergence that triggers "the defined procedure, not an
automatic monetary sanction".

## Decision

- **Operator** (`validator_operators`): the administrative entity that runs one or more validators.
  Every validator belongs to an operator. **Quorum is counted in operators**, never in keys: two keys
  of the same operator never verify the same submission (a constraint enforced in the claim query,
  and tested).
- **Unpredictable audit**: once a submission is recorded, the server draws
  `SHA-256("gpu-goblin/audit-selection/v1\n" || secret || submission_hash) < audit_rate`. The secret
  (32 bytes, `data_dir/audit.secret`, mode 0600) never leaves the server and appears in no response
  and no log. The draw happens **after** the worker has committed its result, and nothing in the
  responses sent to the worker reveals that a submission is being audited.
- **No audit without diversity**: if only one operator can validate, no audit is opened (nobody could
  claim it). A single-operator deployment behaves exactly as before.
- **Combining decisions** (nothing is paid while a verification is pending):

  | Decisions | Outcome |
  |---|---|
  | all `accepted` | accepted, single reward, receipt citing every signed decision |
  | `rejected` with no `accepted` | rejected, reservation released, new attempt if the retry budget allows |
  | `accepted` **and** `rejected` | **dispute**: nothing is paid, nothing is sanctioned, the task waits for an operator |
  | `inconclusive` with `accepted` | `inconclusive`: the favorable opinion is not enough, new attempt |
  | `inconclusive` with `rejected` | rejected |

- **Dispute** (`disputes`): the divergence is recorded with both signed opinions, their validators
  and their operators. The task moves to `disputed`, the budget reservation is **kept**, and the attempt
  stays open. An operator settles the dispute with an explicit decision, recorded in the audit log,
  **and only in favor of an outcome that a validator actually signed**: settlement arbitrates between
  existing opinions, it does not make new ones up. It is this settlement that moves the credits.
- **Receipt**: `ContributionReceiptV1` now carries `decision_hashes`, the list of every signed decision
  recorded on the submission. An audited payment can therefore be verified offline as backed by two
  distinct operators.

## Explicit limits (not to be presented otherwise)

- This **does not make validation "trustless"**. Validators are administered and authorized by the
  control plane operator; the guarantee is "several identified administrative entities recomputed",
  not "nobody needs to trust anybody".
- **Collusion is not solved**: if two operators collude, cross-verification fails. The number of keys
  changes nothing, and neither does reputation. What federation brings is the detection of an
  **isolated** failure or act of complacency, and a signed record of who said what.
- An audit is only a sample: with `audit_rate = r`, a submission has probability `r` of being
  recomputed by a second operator. A dishonest worker gets through with probability `1 - r` per task.
- The cost is real: an audit doubles the verification compute for the drawn submission. `audit_rate`
  is the dial between cost and detection; it is documented, measured in the demo, and defaults to 0.1.
- Algebraic proofs, confidential computing environments and financial penalties remain out of scope
  (brief §11).

## Consequences

- `gg admin operator-create`, `validator-add --operator`, `dispute-list` and `dispute-resolve` expose
  the full cycle; the researcher API shows every signed decision of a task (`verifications`).
- An existing deployment keeps working: already registered validators belong to an operator named
  `unassigned`, and `audit_rate = 0` reproduces the P5 behavior exactly.
- `cargo xtask demo --kind matmul --federated [--faults]` runs the whole path, including a validator
  that accepts without recomputing and the dispute that follows.
