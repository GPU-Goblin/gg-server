# ADR-0030 — Contributor accounts: open sign-up, machines attached by an enrollment code

Status: accepted (2026-09-24).

## Context

Until now every machine was authorized by an operator, one key at a time (`gg admin
worker-authorize`). The beta opens contribution to anyone: people sign up on the site, and their
machines must start working without waiting for an operator. The operator still needs a way to stop
someone quickly, and a contributor needs to see their machines.

## Decision

- **Sign-up by an e-mail code, no admission.** On the portal (`/portal/contribute`) a person enters
  an address and receives an eight-digit code, valid 15 minutes, checked at most five times; the
  address becomes an account on the first correct code. The page answers the same whether or not a
  code was sent (hourly limit per address, shared with researcher sign-in links). Opened by
  `contributor_signup = true`, which needs `[email_login]` to send codes (ADR-0028).
- **An enrollment code attaches machines.** The account's page creates an enrollment code (`gge_…`,
  stored hashed, shown once; a new one replaces the previous). The installer writes it into the
  worker's configuration (`--enroll`).
- **The machine proves its key before it is attached.** Registration alone proves nothing (anyone
  can register any public key). A worker first logs in with its key (signed challenge), then calls
  `POST /worker/enroll` with the code in that session: nobody can attach, or steal, someone else's
  machine. An attached machine is authorized at once; a machine of another account is refused.
- **Suspension stops everything.** `gg admin contributor-suspend <id> --reason … --yes`: every
  machine of the account is deauthorized, its sessions and enrollment codes revoked, and it is no
  longer advertised to peers. Reinstating authorizes the machines again.
- **The account shows its machines**: name, state, last contact, accepted tasks.

## Consequences

- **Correctness does not depend on who contributes.** Every result is recomputed by a validator
  before it is accepted; an audit sample goes to a second operator (ADR-0014). A dishonest machine
  wastes its own time and gets nothing.
- **Anyone who signs up sees the data of the tasks their machine computes.** With the data rule
  `research-any-data`, research data can reach a stranger's machine. The rule shown to researchers
  says so, but open sign-up makes it concrete: sensitive data needs projects restricted to trusted
  machines, which this ADR does not provide (open question for the operator).
- **Sybil accounts are cheap** (one address each). Without rewards of value this only costs the
  attacker's own compute; before any reward of value, admission of contributors or a stake would be
  needed (docs/RISKS.md).
- **Peer-to-peer addresses** of contributors become visible to strangers' machines authorized on the
  same artifacts: the installer and the guide say so.
- Tests: `gg-control/tests/contributors.rs` (sign-up, attempt limit, enrollment with the machine's
  own session, a machine of another account refused, code replacement, suspension and
  reinstatement, sign-up closed by default), `gg-worker/tests/enroll.rs` (the real agent enrolls
  with its own session, then claims).

## Addendum (2026-09-26): the same account as JSON, for the contributor's application

The application (ADR-0043) signs its owner in without a browser, through `/contributor/v1`:
`POST code {email}` (the same answer whether or not a code was sent), `POST session {email, code}`
→ `{session: "ggp_…", expires_in_secs, email}` (403 on a wrong, expired or used code; five checks
per code; the account is created by the first right code), `GET me` → the address, suspension and
machines, `POST enrollment` → a new enrollment code (the previous one stops attaching machines),
`POST logout`. The session is the same kind as the page's cookie, stored hashed, with the same
lifetime (`email_login.session_ttl_secs`), but travels as a bearer token: no cookie, so no
cross-site request can carry it, and these routes sit outside the forms' CSRF layer. They answer
404 while contributor sign-up is closed. Tests: `tests/contributors.rs`
(`the_application_signs_up_and_attaches_a_machine_with_json_alone`, closed by default).
