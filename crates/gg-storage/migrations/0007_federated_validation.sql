-- Federated validation (P6, ADR-0014): several administratively distinct validators, unpredictable
-- audits and a dispute procedure. Multiplicity of keys is not diversity: quorum and audit rules count
-- distinct operators, not validator records.

CREATE TABLE validator_operators (
    id              uuid PRIMARY KEY,
    name            text NOT NULL UNIQUE,
    contact         text NOT NULL DEFAULT '',
    active          boolean NOT NULL DEFAULT true,
    created_at      timestamptz NOT NULL DEFAULT now()
);

-- Existing validators keep working: they are attached to one operator recorded as such.
INSERT INTO validator_operators (id, name, contact)
VALUES ('00000000-0000-7000-8000-000000000001', 'unassigned', '');

ALTER TABLE validators ADD COLUMN operator_id uuid NOT NULL
    REFERENCES validator_operators(id)
    DEFAULT '00000000-0000-7000-8000-000000000001';
CREATE INDEX validators_operator_idx ON validators (operator_id);

-- A submission can now carry several verifications: one primary and, when selected, one audit.
ALTER TABLE verifications DROP CONSTRAINT verifications_submission_id_key;
ALTER TABLE verifications ADD COLUMN kind text NOT NULL DEFAULT 'primary'
    CHECK (kind IN ('primary', 'audit'));
ALTER TABLE verifications ADD CONSTRAINT verifications_submission_kind_key
    UNIQUE (submission_id, kind);

-- Disagreement between decisions on the same submission. Never an automatic sanction: an operator
-- resolves it, and the resolution is what moves credits.
CREATE TABLE disputes (
    id              uuid PRIMARY KEY,
    submission_id   uuid NOT NULL UNIQUE REFERENCES submissions(id),
    task_id         uuid NOT NULL REFERENCES tasks(id),
    attempt_id      uuid NOT NULL REFERENCES attempts(id),
    state           text NOT NULL CHECK (state IN ('open', 'resolved')),
    outcomes        jsonb NOT NULL,              -- [{validator_id, operator_id, outcome, decision_id}]
    resolution      text CHECK (resolution IN ('accepted', 'rejected', 'inconclusive')),
    resolved_by     text NOT NULL DEFAULT '',
    reason          text NOT NULL DEFAULT '',
    created_at      timestamptz NOT NULL DEFAULT now(),
    resolved_at     timestamptz
);
CREATE INDEX disputes_open_idx ON disputes (created_at) WHERE state = 'open';

-- A task can now wait for an operator's decision after a disagreement.
ALTER TABLE tasks DROP CONSTRAINT tasks_state_check;
ALTER TABLE tasks ADD CONSTRAINT tasks_state_check CHECK (state IN
    ('queued','assigned','submitted','accepted','rejected','inconclusive','disputed','failed','cancelled'));
