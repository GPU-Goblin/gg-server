-- Contributor accounts (ADR-0030). Anyone may sign up with an e-mail address, proven by a code sent
-- to it; there is no admission. Machines registered with the account's enrollment token are
-- attached to it and authorized automatically; suspending the account stops all of them.
CREATE TABLE contributors (
    id               uuid PRIMARY KEY,
    email            text NOT NULL CHECK (length(email) BETWEEN 3 AND 254),
    created_at       timestamptz NOT NULL DEFAULT now(),
    verified_at      timestamptz NOT NULL,
    suspended_at     timestamptz,
    suspended_reason text NOT NULL DEFAULT ''
);
CREATE UNIQUE INDEX contributors_email_idx ON contributors (lower(email));

-- Verification codes sent by e-mail (hashed with the address), short-lived, few attempts.
CREATE TABLE contributor_codes (
    id          uuid PRIMARY KEY,
    email       text NOT NULL,
    code_hash   bytea NOT NULL CHECK (octet_length(code_hash) = 32),
    created_at  timestamptz NOT NULL DEFAULT now(),
    expires_at  timestamptz NOT NULL,
    consumed_at timestamptz,
    attempts    integer NOT NULL DEFAULT 0
);
CREATE INDEX contributor_codes_email_idx ON contributor_codes (lower(email), created_at);

-- Browser sessions of a contributor on the portal (token hashed).
CREATE TABLE contributor_sessions (
    session_hash   bytea PRIMARY KEY CHECK (octet_length(session_hash) = 32),
    contributor_id uuid NOT NULL REFERENCES contributors(id),
    created_at     timestamptz NOT NULL DEFAULT now(),
    expires_at     timestamptz NOT NULL,
    revoked_at     timestamptz
);

-- Enrollment tokens: given to gg-worker at install time to attach the machine to the account.
CREATE TABLE enrollment_tokens (
    token_hash     bytea PRIMARY KEY CHECK (octet_length(token_hash) = 32),
    contributor_id uuid NOT NULL REFERENCES contributors(id),
    created_at     timestamptz NOT NULL DEFAULT now(),
    revoked_at     timestamptz
);
CREATE INDEX enrollment_tokens_contributor_idx ON enrollment_tokens (contributor_id);

ALTER TABLE workers ADD COLUMN contributor_id uuid REFERENCES contributors(id);
CREATE INDEX workers_contributor_idx ON workers (contributor_id);
