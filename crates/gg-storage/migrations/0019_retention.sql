-- Retention of a collective run's intermediate artifacts (ADR-0040): an artifact the retention
-- policy removed keeps its row (hash, size, owner) with the time it expired; a round whose
-- intermediate artifacts were examined is marked, so that each round is examined once.
ALTER TABLE artifacts ADD COLUMN expired_at timestamptz;
ALTER TABLE collective_rounds ADD COLUMN pruned_at timestamptz;
CREATE INDEX collective_rounds_prunable_idx ON collective_rounds (closed_at)
    WHERE pruned_at IS NULL AND state IN ('aggregated', 'failed');
