-- Sign-in by a verified e-mail address, for researchers without ORCID (a university address, for
-- instance). Signing in proves control of the address; admission stays a decision of an operator.
ALTER TABLE researchers
    ADD COLUMN email             text CHECK (email IS NULL OR (length(email) BETWEEN 3 AND 254)),
    ADD COLUMN email_verified_at timestamptz;
CREATE UNIQUE INDEX researchers_email_idx ON researchers (lower(email)) WHERE email IS NOT NULL;

-- Single-use sign-in links (token hashed), short-lived.
CREATE TABLE email_login_links (
    token_hash  bytea PRIMARY KEY CHECK (octet_length(token_hash) = 32),
    email       text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    expires_at  timestamptz NOT NULL,
    consumed_at timestamptz
);
CREATE INDEX email_login_links_email_idx ON email_login_links (lower(email), created_at);
