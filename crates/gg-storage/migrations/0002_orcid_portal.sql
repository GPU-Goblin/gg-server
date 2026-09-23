-- ORCID authentication and portal sessions (P3).

ALTER TABLE researchers
    ADD COLUMN orcid_name text NOT NULL DEFAULT '',
    ADD COLUMN orcid_linked_at timestamptz;

-- Single-use OAuth `state` values, bound to a pre-login browser cookie.
CREATE TABLE oauth_states (
    state_hash      bytea PRIMARY KEY CHECK (octet_length(state_hash) = 32),
    cookie_hash     bytea NOT NULL CHECK (octet_length(cookie_hash) = 32),
    provider        text NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    expires_at      timestamptz NOT NULL,
    consumed_at     timestamptz
);

-- Browser sessions of the portal (token hashed).
CREATE TABLE portal_sessions (
    session_hash    bytea PRIMARY KEY CHECK (octet_length(session_hash) = 32),
    researcher_id   uuid NOT NULL REFERENCES researchers(id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    expires_at      timestamptz NOT NULL,
    revoked_at      timestamptz,
    user_agent      text NOT NULL DEFAULT ''
);
CREATE INDEX portal_sessions_researcher_idx ON portal_sessions (researcher_id);
