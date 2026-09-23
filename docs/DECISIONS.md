# Operational decisions

Decisions taken by the project lead on 2026-09-17, in response to the open points in
docs/RISKS.md. They drive the technical work; this document states what was decided, what follows
from it and what remains to be done.

## D1 — Goal: public release, blockers resolved first

**Decided**: aim for a release open to unknown contributors, after the four blockers have been
addressed (R1 collusion between operators, R2 server key rotation, R3 reproducible binaries,
R4 independent review).

Consequences:

- No public release as long as the four blockers are neither addressed nor explicitly accepted in
  writing. The repository's default status remains "not ready" (docs/RISKS.md).
- Default configuration choices now target public mode, not local convenience: `public = true`
  rejects development mode, the ORCID simulator, injected faults, disabled isolation and advertised
  private addresses.
- R4 is external by construction: it requires a third party, not code. The review material is
  ready (docs/REVIEW.md).

## D2 — Audit rate: 0.25 initially, reconfigurable

**Decided**: start at 0.25 (one accepted result in four re-verified by an administratively
distinct operator), with the ability to change this value **without redeploying**.

Consequences:

- Default value raised to 0.25 (previously 0.1).
- Configurable in three ways: `audit_rate` in the server file, `GG_AUDIT_RATE` in the environment,
  and `POST /v1/admin/config/audit-rate` at runtime (recorded in the audit log).
- Expected cost: about 2.25 times the useful computation per accepted result, versus 2 without
  audits (docs/ECONOMICS.md). To be reassessed against real measurements of cheating, not
  intuition.

**Done**: default value 0.25; `GET`/`POST /v1/admin/config/audit-rate` routes and the
`gg admin audit-rate [--set 0.5 --reason "..."]` command; changes recorded in the audit log; test
`federated.rs::the_audit_rate_is_changed_while_the_server_runs` (switching from 0 to 1 at runtime,
out-of-range values rejected).

## D3 — Release key: dedicated offline machine

**Decided**: the key that signs executables never resides on a networked machine or on the build
machine.

Consequences:

- Releasing is split in two: `cargo xtask release --unsigned` builds and computes the digests on
  the build machine; `gg release sign` signs the manifest on the offline machine; the signed
  manifest comes back on removable media.
- The offline machine only needs the `gg` binary and the key file.
- Still to be decided: who physically holds the machine and the key, and who can revoke
  (docs/RELEASE.md).

**Done**: `cargo xtask release` no longer signs (unsigned manifest); `gg release sign` signs
offline; procedure verified end to end on 2026-09-17 and described in docs/RELEASE.md.

## D4 — Admitted data: synthetic and public only

**Decided**: only synthetic or already public data may enter the platform.

Consequences:

- The rule is consistent with the architecture: the owner of the computing machine **sees** the
  data it processes, and no confidentiality can be enforced against them.
- The rule is published by the server (`/v1/info`), displayed in the portal, and a project can
  only be created with an explicit, recorded acceptance.
- Any exception would require confidential computing, which is outside the project's scope.

**Done**: `data_policy` in the server configuration (default `public-synthetic-only`), published in
plain text by `GET /v1/info` (`data_policy_text`); a project can only be created with an explicit
acceptance (`--ack-data-policy`), recorded on the project and in the audit log; test
`e2e.rs::a_project_exists_only_once_the_data_rule_is_accepted`.

### D4, revised on 2026-09-24 for research deployments

**Decided**: a research deployment accepts any kind of research data, and says plainly that
confidentiality calls for caution. The data rule `research-any-data` is published by the server
and must be accepted when a project is created, like the previous one. Its text: the owner of the
machine doing the computation sees the data it processes, the platform offers no confidentiality
with respect to contributors, and personal data (voices, health records…) remain the
responsibility of the researcher and of their institution's data protection officer.

`public-synthetic-only` stays the default of the configuration; a deployment chooses
`data_policy = "research-any-data"` explicitly. Nothing in the architecture changed: the reason
for caution is the same as the reason for the original rule.

## Decisions still open

Carried over from docs/RISKS.md, unanswered to date:

1. Who operates the control plane (people, on-call rotation, liability in case of an incident).
2. Who can be a validation operator, how their administrative diversity is established, and what
   happens when collusion is detected (blocker R1).
3. Who holds the release machine and key, and who can revoke (complement to D3).
4. Legal scope: liability for computation run on third-party machines, contributors'
   jurisdictions.
5. Whether there will ever be a token: chain, security audit, governance, legal review.
