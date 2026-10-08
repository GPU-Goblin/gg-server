-- Micro-batch rows cut by a lab node (ADR-0041, the cutting by the node): for each step of a run it
-- steps, one map per task of the dataset name to the rows artifact the node holds. The server only
-- checks their sizes and holder; a round replayed at the same step reuses them.
CREATE TABLE node_round_rows (
    run_id uuid NOT NULL REFERENCES collective_runs(id) ON DELETE CASCADE,
    step integer NOT NULL CHECK (step >= 0),
    rows jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (run_id, step)
);
