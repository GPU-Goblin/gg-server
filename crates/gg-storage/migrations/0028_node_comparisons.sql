-- Comparisons by a lab node (ADR-0042): a node compares the computations of the jobs it holds. A
-- node's comparison that finds a difference counts only once the server has replayed it (on the
-- two outputs, fetched from the node for that purpose and not kept); the replay is recorded here.
ALTER TABLE comparisons
    ADD COLUMN replayed_outcome text CHECK (replayed_outcome IN ('accepted', 'rejected', 'inconclusive')),
    ADD COLUMN replayed_hash bytea,
    ADD COLUMN replayed_cbor bytea;

-- A node found to have misreported a comparison is suspended pending a human: its comparisons are
-- no longer taken, and the server compares its jobs' computations itself.
ALTER TABLE lab_nodes
    ADD COLUMN suspended_at timestamptz,
    ADD COLUMN suspended_reason text NOT NULL DEFAULT '';
