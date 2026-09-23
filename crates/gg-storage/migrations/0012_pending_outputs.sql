-- Activations handed on before their durable copy exists (ADR-0026, a bounded exception to
-- invariant I7 of docs/SPEC.md). The producer declares the hash, the size and the chunk digests of
-- an `out.*` or `din.*` output of a `graph.stage.v1` task; the server signs a manifest over those
-- digests and the next stage fetches the output from the producer. The row disappears when the
-- durable copy arrives. Nothing is verified while it exists; once `deadline` has passed, the round
-- that handed the output on is poisoned.
CREATE TABLE pending_outputs (
    hash        bytea NOT NULL CHECK (octet_length(hash) = 32),
    attempt_id  uuid NOT NULL REFERENCES attempts(id),
    worker_id   uuid NOT NULL REFERENCES workers(id),
    project_id  uuid NOT NULL REFERENCES projects(id),
    size        bigint NOT NULL CHECK (size >= 0),
    chunk_size  integer NOT NULL CHECK (chunk_size > 0),
    chunk_count integer NOT NULL CHECK (chunk_count >= 0),
    digests     bytea NOT NULL CHECK (octet_length(digests) = chunk_count * 32),
    deadline    timestamptz NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (hash, attempt_id)
);
CREATE INDEX pending_outputs_attempt_idx ON pending_outputs (attempt_id);

-- A producer announces itself as holder of an activation before the server stores it, so the
-- announcement can no longer require an `artifacts` row. The server still checks the right to
-- announce.
ALTER TABLE artifact_peers DROP CONSTRAINT artifact_peers_hash_fkey;
