-- Tasks being computed or awaiting verification, most recent first: validators prefetch their
-- inputs (ADR-0037). Partial, so it stays as small as the work in flight.
CREATE INDEX tasks_upcoming_idx ON tasks (updated_at DESC) WHERE state IN ('assigned', 'submitted');
