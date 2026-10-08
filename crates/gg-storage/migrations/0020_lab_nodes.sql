-- Lab nodes (ADR-0041): a researcher's machine that holds, serves and aggregates the artifacts of
-- its runs. Registered by the researcher with its own key; signs in by challenge like a worker.
CREATE TABLE lab_nodes (
    id              uuid PRIMARY KEY,
    researcher_id   uuid NOT NULL REFERENCES researchers(id),
    public_key      bytea NOT NULL UNIQUE CHECK (octet_length(public_key) = 32),
    display_name    text NOT NULL DEFAULT '',
    created_at      timestamptz NOT NULL DEFAULT now(),
    revoked_at      timestamptz,
    last_seen_at    timestamptz
);
CREATE INDEX lab_nodes_researcher_idx ON lab_nodes (researcher_id);

ALTER TABLE sessions DROP CONSTRAINT sessions_principal_kind_check;
ALTER TABLE sessions ADD CONSTRAINT sessions_principal_kind_check
    CHECK (principal_kind IN ('worker', 'validator', 'node'));

ALTER TABLE login_challenges DROP CONSTRAINT login_challenges_action_check;
ALTER TABLE login_challenges ADD CONSTRAINT login_challenges_action_check
    CHECK (action IN ('worker-login', 'validator-login', 'node-login'));
