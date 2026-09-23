-- GPU Goblin initial schema (P1). PostgreSQL 17.
-- Conventions: ids are UUIDs, states are text constrained by CHECK, times are timestamptz set by the server.

CREATE TABLE researchers (
    id              uuid PRIMARY KEY,
    display_name    text NOT NULL,
    orcid           text UNIQUE,                  -- NULL until ORCID login (P3)
    admission_state text NOT NULL CHECK (admission_state IN ('pending','approved','suspended','rejected')),
    admission_note  text NOT NULL DEFAULT '',
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

-- Bearer tokens (hashed) for researchers and admins. Worker/validator sessions live in `sessions`.
CREATE TABLE api_tokens (
    token_hash      bytea PRIMARY KEY,            -- SHA-256 of the token
    role            text NOT NULL CHECK (role IN ('admin','researcher')),
    researcher_id   uuid REFERENCES researchers(id),
    label           text NOT NULL DEFAULT '',
    created_at      timestamptz NOT NULL DEFAULT now(),
    expires_at      timestamptz,
    revoked_at      timestamptz,
    CHECK ((role = 'researcher') = (researcher_id IS NOT NULL))
);

CREATE TABLE projects (
    id              uuid PRIMARY KEY,
    name            text NOT NULL,
    state           text NOT NULL CHECK (state IN ('active','suspended','closed')),
    allowed_kinds   text[] NOT NULL DEFAULT '{}',  -- compute kinds the project may submit
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE project_memberships (
    project_id      uuid NOT NULL REFERENCES projects(id),
    researcher_id   uuid NOT NULL REFERENCES researchers(id),
    role            text NOT NULL CHECK (role IN ('owner','member','viewer')),
    created_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (project_id, researcher_id)
);

-- Materialized balances; every change is accompanied by balanced ledger entries in the same transaction.
CREATE TABLE project_budgets (
    project_id      uuid PRIMARY KEY REFERENCES projects(id),
    available       bigint NOT NULL DEFAULT 0 CHECK (available >= 0),
    reserved        bigint NOT NULL DEFAULT 0 CHECK (reserved >= 0),
    spent           bigint NOT NULL DEFAULT 0 CHECK (spent >= 0),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE workers (
    id              uuid PRIMARY KEY,
    public_key      bytea NOT NULL UNIQUE CHECK (octet_length(public_key) = 32),
    display_name    text NOT NULL DEFAULT '',
    authorized      boolean NOT NULL DEFAULT false,
    payout_address  text,                          -- separate from the technical key; P7
    created_at      timestamptz NOT NULL DEFAULT now(),
    last_seen_at    timestamptz
);

CREATE TABLE validators (
    id              uuid PRIMARY KEY,
    public_key      bytea NOT NULL UNIQUE CHECK (octet_length(public_key) = 32),
    display_name    text NOT NULL DEFAULT '',
    authorized      boolean NOT NULL DEFAULT false,
    created_at      timestamptz NOT NULL DEFAULT now(),
    last_seen_at    timestamptz
);

-- Single-use login challenges for workers/validators.
CREATE TABLE login_challenges (
    nonce           bytea PRIMARY KEY CHECK (octet_length(nonce) = 32),
    action          text NOT NULL CHECK (action IN ('worker-login','validator-login')),
    public_key      bytea NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    expires_at      timestamptz NOT NULL,
    consumed_at     timestamptz
);

-- Short-lived sessions for workers/validators (token hashed).
CREATE TABLE sessions (
    token_hash      bytea PRIMARY KEY,
    principal_kind  text NOT NULL CHECK (principal_kind IN ('worker','validator')),
    principal_id    uuid NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    expires_at      timestamptz NOT NULL,
    revoked_at      timestamptz
);

CREATE TABLE artifacts (
    hash            bytea PRIMARY KEY CHECK (octet_length(hash) = 32),
    size            bigint NOT NULL CHECK (size >= 0),
    media_type      text NOT NULL,
    schema          text NOT NULL DEFAULT '',
    visibility      text NOT NULL CHECK (visibility IN ('project','public')),
    license         text NOT NULL DEFAULT '',
    retention       text NOT NULL DEFAULT 'standard',
    project_id      uuid REFERENCES projects(id),   -- owner project (NULL for system artifacts)
    uploaded_by     text NOT NULL DEFAULT '',        -- principal description (no secrets)
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE experiments (
    id              uuid PRIMARY KEY,
    project_id      uuid NOT NULL REFERENCES projects(id),
    name            text NOT NULL,
    created_by      uuid REFERENCES researchers(id),
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE jobs (
    id              uuid PRIMARY KEY,
    project_id      uuid NOT NULL REFERENCES projects(id),
    experiment_id   uuid NOT NULL REFERENCES experiments(id),
    submitted_by    uuid REFERENCES researchers(id),
    name            text NOT NULL DEFAULT '',
    cancelled       boolean NOT NULL DEFAULT false,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE tasks (
    id              uuid PRIMARY KEY,
    job_id          uuid NOT NULL REFERENCES jobs(id),
    project_id      uuid NOT NULL REFERENCES projects(id),
    experiment_id   uuid NOT NULL REFERENCES experiments(id),
    index_in_job    integer NOT NULL,
    compute_kind    text NOT NULL,
    spec_hash       bytea NOT NULL CHECK (octet_length(spec_hash) = 32),
    definition_hash bytea NOT NULL CHECK (octet_length(definition_hash) = 32),
    spec_cbor       bytea NOT NULL,                 -- canonical TaskSpecV1
    state           text NOT NULL CHECK (state IN ('queued','assigned','submitted','accepted','rejected','inconclusive','failed','cancelled')),
    generation      bigint NOT NULL DEFAULT 0,      -- monotonic; bumped on every assignment
    attempts_made   integer NOT NULL DEFAULT 0,
    max_attempts    integer NOT NULL,
    priority        integer NOT NULL DEFAULT 0,
    reward_primary  bigint NOT NULL CHECK (reward_primary >= 0),
    verification_cost bigint NOT NULL CHECK (verification_cost >= 0),
    est_cpu_secs    double precision NOT NULL DEFAULT 0,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (job_id, index_in_job)
);
CREATE INDEX tasks_queue_idx ON tasks (state, priority DESC, created_at) WHERE state = 'queued';
CREATE INDEX tasks_job_idx ON tasks (job_id);

CREATE TABLE attempts (
    id              uuid PRIMARY KEY,
    task_id         uuid NOT NULL REFERENCES tasks(id),
    worker_id       uuid NOT NULL REFERENCES workers(id),
    generation      bigint NOT NULL,
    role            text NOT NULL CHECK (role IN ('primary','replica','audit')),
    state           text NOT NULL CHECK (state IN ('active','submitted','expired','abandoned','superseded','accepted','rejected','inconclusive')),
    assignment_cbor bytea NOT NULL,                 -- Signed<AssignmentV1>
    assignment_hash bytea NOT NULL CHECK (octet_length(assignment_hash) = 32),
    reservation_id  uuid,                           -- set below (FK added after budget_reservations)
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (task_id, generation)
);
CREATE INDEX attempts_worker_idx ON attempts (worker_id, state);

CREATE TABLE leases (
    id              uuid PRIMARY KEY,
    attempt_id      uuid NOT NULL UNIQUE REFERENCES attempts(id),
    expires_at      timestamptz NOT NULL,
    max_until       timestamptz NOT NULL,
    renewals        integer NOT NULL DEFAULT 0,
    released_at     timestamptz
);
CREATE INDEX leases_expiry_idx ON leases (expires_at) WHERE released_at IS NULL;

CREATE TABLE budget_reservations (
    id              uuid PRIMARY KEY,
    project_id      uuid NOT NULL REFERENCES projects(id),
    task_id         uuid NOT NULL REFERENCES tasks(id),
    attempt_id      uuid NOT NULL UNIQUE REFERENCES attempts(id),
    amount          bigint NOT NULL CHECK (amount >= 0),
    state           text NOT NULL CHECK (state IN ('reserved','committed','released')),
    created_at      timestamptz NOT NULL DEFAULT now(),
    resolved_at     timestamptz
);
ALTER TABLE attempts ADD CONSTRAINT attempts_reservation_fk FOREIGN KEY (reservation_id) REFERENCES budget_reservations(id);

CREATE TABLE submissions (
    id              uuid PRIMARY KEY,
    task_id         uuid NOT NULL REFERENCES tasks(id),
    attempt_id      uuid NOT NULL UNIQUE REFERENCES attempts(id),
    generation      bigint NOT NULL,
    submission_hash bytea NOT NULL UNIQUE CHECK (octet_length(submission_hash) = 32),
    submission_cbor bytea NOT NULL,                 -- Signed<ResultSubmissionV1>
    created_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (task_id, generation)                    -- one canonical result per generation (I4)
);

CREATE TABLE verifications (
    id              uuid PRIMARY KEY,
    submission_id   uuid NOT NULL UNIQUE REFERENCES submissions(id),
    task_id         uuid NOT NULL REFERENCES tasks(id),
    attempt_id      uuid NOT NULL REFERENCES attempts(id),
    state           text NOT NULL CHECK (state IN ('pending','in_progress','decided')),
    validator_id    uuid REFERENCES validators(id),
    lease_expires_at timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX verifications_pending_idx ON verifications (created_at) WHERE state <> 'decided';

CREATE TABLE decisions (
    id              uuid PRIMARY KEY,
    verification_id uuid NOT NULL UNIQUE REFERENCES verifications(id),
    validator_id    uuid NOT NULL REFERENCES validators(id),
    decision_hash   bytea NOT NULL UNIQUE CHECK (octet_length(decision_hash) = 32),
    decision_cbor   bytea NOT NULL,                 -- Signed<VerificationDecisionV1>
    outcome         text NOT NULL CHECK (outcome IN ('accepted','rejected','inconclusive')),
    created_at      timestamptz NOT NULL DEFAULT now()
);

-- Append-only double-entry ledger.
CREATE TABLE ledger_transactions (
    id              uuid PRIMARY KEY,
    kind            text NOT NULL,                  -- grant, reserve, release, commit, reward, correction
    reward_key      text UNIQUE,                    -- non-null only for reward postings (I5)
    reference       text NOT NULL DEFAULT '',       -- e.g. attempt id, project id
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE ledger_entries (
    id              bigserial PRIMARY KEY,
    txn_id          uuid NOT NULL REFERENCES ledger_transactions(id),
    account         text NOT NULL,                  -- project:<id>:available|reserved|spent, worker:<id>:earned, system:grants, system:verification
    debit           bigint NOT NULL DEFAULT 0 CHECK (debit >= 0),
    credit          bigint NOT NULL DEFAULT 0 CHECK (credit >= 0),
    CHECK (debit = 0 OR credit = 0)
);
CREATE INDEX ledger_entries_account_idx ON ledger_entries (account);
CREATE INDEX ledger_entries_txn_idx ON ledger_entries (txn_id);

-- Every transaction must balance at commit time.
CREATE OR REPLACE FUNCTION ledger_check_balance() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    d bigint; c bigint;
BEGIN
    SELECT coalesce(sum(debit),0), coalesce(sum(credit),0) INTO d, c FROM ledger_entries WHERE txn_id = NEW.txn_id;
    IF d <> c THEN
        RAISE EXCEPTION 'ledger transaction % is unbalanced (debit %, credit %)', NEW.txn_id, d, c;
    END IF;
    RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER ledger_balance_trg AFTER INSERT OR UPDATE OR DELETE ON ledger_entries
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION ledger_check_balance();

-- Ledger is append-only.
CREATE OR REPLACE FUNCTION ledger_no_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'ledger tables are append-only';
END $$;
CREATE TRIGGER ledger_entries_immutable BEFORE UPDATE OR DELETE ON ledger_entries FOR EACH ROW EXECUTE FUNCTION ledger_no_mutation();
CREATE TRIGGER ledger_txn_immutable BEFORE UPDATE OR DELETE ON ledger_transactions FOR EACH ROW EXECUTE FUNCTION ledger_no_mutation();

CREATE TABLE receipts (
    id              uuid PRIMARY KEY,
    attempt_id      uuid NOT NULL REFERENCES attempts(id),
    worker_id       uuid NOT NULL REFERENCES workers(id),
    project_id      uuid NOT NULL REFERENCES projects(id),
    role            text NOT NULL,
    amount          bigint NOT NULL CHECK (amount >= 0),
    reward_state    text NOT NULL CHECK (reward_state IN ('none','earned','batched','settled')),
    ledger_txn_id   uuid REFERENCES ledger_transactions(id),
    receipt_hash    bytea NOT NULL UNIQUE,
    receipt_cbor    bytea NOT NULL,                 -- Signed<ContributionReceiptV1>
    created_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (attempt_id, role)                       -- one reward per attempt and role
);

CREATE TABLE idempotency_keys (
    actor           text NOT NULL,
    operation       text NOT NULL,
    key             text NOT NULL,
    request_hash    bytea NOT NULL,
    status          integer NOT NULL,
    response        jsonb NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (actor, operation, key)
);

CREATE TABLE outbox (
    id              bigserial PRIMARY KEY,
    topic           text NOT NULL,
    payload         jsonb NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    delivered_at    timestamptz
);
CREATE INDEX outbox_pending_idx ON outbox (id) WHERE delivered_at IS NULL;

CREATE TABLE audit_events (
    id              uuid PRIMARY KEY,
    actor           text NOT NULL,
    action          text NOT NULL,
    subject         text NOT NULL,
    detail          jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now()
);
