-- Comparisons checked by contributors (ADR-0042, owner's decision of 2026-09-26): when a lab node
-- reports that two computations differ, the server does not read their outputs; a
-- `replica.compare.v1` task compares them, computed by a contributor of an account foreign to the
-- task, drawn by the server. It names the family and the two attempts it compares.
ALTER TABLE tasks
    ADD COLUMN compare_family uuid REFERENCES tasks(id),
    ADD COLUMN compare_a uuid REFERENCES attempts(id),
    ADD COLUMN compare_b uuid REFERENCES attempts(id),
    ADD CONSTRAINT tasks_compare_all_or_none CHECK (
        (compare_family IS NULL) = (compare_a IS NULL) AND (compare_a IS NULL) = (compare_b IS NULL));
CREATE INDEX tasks_compare_family ON tasks (compare_family) WHERE compare_family IS NOT NULL;

-- The server no longer replays a node's comparison itself: the replay is a comparer's submission.
ALTER TABLE comparisons RENAME COLUMN replayed_cbor TO replayed_by;
ALTER TABLE comparisons ALTER COLUMN replayed_by TYPE text USING '';
