-- Verification by other contributors (ADR-0042): every task is computed twice, by two workers of
-- different contributor accounts, and the two results are compared.
--
-- The second computation is a task of its own (a "twin"), in a twin job of the first one's job:
-- same definition, its own generation and attempts, invisible to rounds and stages, which only read
-- the first job. A worker cannot tell a twin from a first computation. `replica_seq` is 1 for the
-- twin, 2 for the third computation that settles a disagreement.
ALTER TABLE jobs
    ADD COLUMN replica_of uuid REFERENCES jobs(id),
    ADD COLUMN replica_seq smallint NOT NULL DEFAULT 0;
-- One twin job per rank: the replicas of a job's tasks, and their third computations.
CREATE UNIQUE INDEX jobs_one_replica_job ON jobs (replica_of, replica_seq) WHERE replica_of IS NOT NULL;

ALTER TABLE tasks
    ADD COLUMN replica_of uuid REFERENCES tasks(id),
    ADD COLUMN replica_seq smallint NOT NULL DEFAULT 0 CHECK (replica_seq BETWEEN 0 AND 2),
    -- The worker the server drew for this computation, alone to be offered it until
    -- `designated_until`; any worker of another account afterwards.
    ADD COLUMN designated_worker uuid REFERENCES workers(id),
    ADD COLUMN designated_until timestamptz,
    ADD CONSTRAINT tasks_replica_seq CHECK ((replica_of IS NULL) = (replica_seq = 0));
CREATE UNIQUE INDEX tasks_one_replica_per_seq ON tasks (replica_of, replica_seq)
    WHERE replica_of IS NOT NULL;

-- Whether a worker serves its outputs to peers, as announced at its last claim: a twin of a job a
-- lab node holds is drawn among those only.
ALTER TABLE workers ADD COLUMN announced_serves_peers boolean NOT NULL DEFAULT false;

-- The comparisons of two computations of one task, signed by whoever compared them (the server,
-- or the lab node that pulled both). `family` is the first computation's task.
CREATE TABLE comparisons (
    id              uuid PRIMARY KEY,
    family          uuid NOT NULL REFERENCES tasks(id),
    attempt_a       uuid NOT NULL REFERENCES attempts(id),
    attempt_b       uuid NOT NULL REFERENCES attempts(id),
    comparer        text NOT NULL,
    outcome         text NOT NULL CHECK (outcome IN ('accepted', 'rejected', 'inconclusive')),
    comparison_hash bytea NOT NULL,
    signed_cbor     bytea NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (attempt_a, attempt_b),
    CHECK (attempt_a <> attempt_b)
);
CREATE INDEX comparisons_family ON comparisons (family);

-- A dispute may also come from comparisons (no majority among three computations): one per
-- computation's submission, its `outcomes` naming the comparisons.
