# ADR-0010 — Researcher authentication with ORCID (server-side Authorization Code flow)

Status: accepted (2026-09-16) — the real provider is **untested** until Sandbox credentials are provided.

## Context

Researcher access goes through ORCID. Authentication (who is this person?) and scientific authorization
(admission, project membership, budget) are two separate decisions (docs/SPEC.md §1).

## Decision

- **OAuth 2.0 Authorization Code** flow run on the server (`gg-server`), following the ORCID documentation
  "Get an authenticated ORCID iD" as consulted on 2026-09-16:
  authorization `https://sandbox.orcid.org/oauth/authorize` (production: `https://orcid.org/oauth/authorize`),
  token `https://sandbox.orcid.org/oauth/token`; parameters `client_id`, `response_type=code`, `scope`,
  `redirect_uri`; exchange `grant_type=authorization_code`, `code`, `client_id`, `client_secret`,
  `redirect_uri`; response `access_token`, `token_type`, `refresh_token`, `expires_in`, `scope`, `name`, `orcid`.
- Minimal scope `/authenticate` (the `openid` alternative was not chosen: we do not need an `id_token`;
  the iD and the name arrive in the token response over the server↔ORCID TLS channel).
  No write scope.
- **PKCE is not used**: the ORCID documentation we consulted mentions neither `code_challenge` nor
  `code_verifier`, and we do not assume support that is not documented. Protection against code injection
  relies on the single-use `state` (32 random bytes, stored hashed, 10-minute expiry, bound to the
  browser session) and on the closed list of `redirect_uri` values registered in the configuration.
- The ORCID access token is **not kept** after the exchange (only the iD, the name and the timestamp are
  recorded); it is never passed to the compute client and never logged.
- Server-side sessions: `gg_session` cookie (`HttpOnly`, `SameSite=Lax`, `Secure` outside `dev_mode`,
  `Path=/`), random identifier stored hashed, 12-hour lifetime, revocation (`/portal/logout`, admin).
  Portal mutations require a `Sec-Fetch-Site` header of same-origin/none (tower-http `csrf`).
- After the first sign-in, a `Researcher` is created with admission `pending`; an operator approves it
  (`gg admin researcher-admission`). A researcher API token can then be issued from the portal
  (shown once) for the CLI.
- `dev_mode`: an in-process **simulated** provider (`/dev/orcid/authorize` immediately returns a code
  for an iD chosen in the form) — labeled as such on the portal and refused in `public` mode.

## Consequences

- The real provider stays marked "untested" in docs/STATUS.md until a trial with Sandbox credentials;
  the implementation is exercised end to end with the simulator.
- The `gg-control` crate gains a `portal` module (minimal server-rendered HTML) and an `orcid` module.
