# ADR-0028 — Researcher sign-in by e-mail link

Status: accepted (2026-09-24). Complements ADR-0010 (ORCID).

## Context

A research lab wants its members to sign in with their institutional address; not every
researcher has an ORCID iD, and a first beta should not depend on an ORCID client registration.
As with ORCID, signing in only answers "who is this?"; admission, projects and budget remain
decisions of an operator (docs/SPEC.md §1).

## Decision

- **A single-use link sent by e-mail.** The researcher enters an address; if its domain is
  accepted, the server stores a random token (hashed, never in clear), valid 15 minutes by
  default, and mails a link `public_url/portal/email/confirm?token=…`.
- **Opening the link does not sign in; a button does.** Mail security scanners fetch links
  automatically; a `GET` only shows a form, and the `POST` consumes the token. A consumed or
  expired token is refused.
- **An account per address, pending admission.** The first sign-in creates a researcher in
  `pending` admission with the verified address; later sign-ins reuse it. The operator sees the
  address in `gg admin researcher-list` and admits by hand.
- **Accepted domains** (`allowed_domains`): a domain and its subdomains; an empty list accepts any
  address, which is reasonable only because admission is manual.
- **No account enumeration.** The page answers the same whether a link was sent, whether the
  address is known, and whether the hourly limit per address (`max_links_per_hour`) was reached.
- **Transport.** SMTP through `lettre`, STARTTLS or TLS with system root certificates; plain SMTP
  only to a relay on this machine. The password comes from an environment variable. A simulated
  mailer exists for development only (`dev_mode`, refused on public deployments), with a mailbox
  page.

## Consequences

- Controlling a mailbox is enough to sign in as its owner: the security of the account is that of
  the institution's mail. Manual admission limits what an attacker gains.
- The deployment needs an outgoing mail relay (the institution's, or a sending service): a decision
  for the operator, and a secret to hold.
- Tests: `tests/portal.rs` covers the simulated flow (domains, single use, expiry, rate limit, one
  account per address) and the real SMTP path against a minimal relay on this machine.
