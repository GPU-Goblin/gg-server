-- Collective training runs (P4, ADR-0011).

CREATE TABLE collective_runs (
    id              uuid PRIMARY KEY,
    project_id      uuid NOT NULL REFERENCES projects(id),
    experiment_id   uuid NOT NULL REFERENCES experiments(id),
    created_by      uuid REFERENCES researchers(id),
    name            text NOT NULL DEFAULT '',
    state           text NOT NULL CHECK (state IN ('running','completed','failed','cancelled')),
    config          jsonb NOT NULL,                 -- CollectiveConfig
    step            integer NOT NULL DEFAULT 0,     -- rounds aggregated so far
    checkpoint      jsonb NOT NULL,                 -- current CheckpointRefs (weights + optimizer state)
    failure         text NOT NULL DEFAULT '',
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE collective_rounds (
    id              uuid PRIMARY KEY,
    run_id          uuid NOT NULL REFERENCES collective_runs(id),
    step            integer NOT NULL,
    attempt         integer NOT NULL DEFAULT 1,
    round_seed      bigint NOT NULL,
    job_id          uuid NOT NULL REFERENCES jobs(id),
    state           text NOT NULL CHECK (state IN ('open','aggregated','failed')),
    expected_examples bigint NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    closed_at       timestamptz,
    UNIQUE (run_id, step, attempt)
);
CREATE INDEX collective_rounds_open_idx ON collective_rounds (created_at) WHERE state = 'open';

-- One published checkpoint per (run, step): aggregation is idempotent.
CREATE TABLE collective_checkpoints (
    run_id          uuid NOT NULL REFERENCES collective_runs(id),
    step            integer NOT NULL,
    checkpoint      jsonb NOT NULL,
    from_round      uuid REFERENCES collective_rounds(id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (run_id, step)
);
