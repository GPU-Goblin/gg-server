-- Peer-assisted artifact distribution (P5, ADR-0012). The control plane keeps the durable copy and
-- stays the authority: it publishes the chunk digests it computed itself over its own bytes, and it
-- knows which worker announced holding which artifact. Peers distribute bytes, nothing else.

-- Chunk digests of an artifact, computed once from the stored object.
CREATE TABLE artifact_chunks (
    hash        bytea PRIMARY KEY REFERENCES artifacts(hash),
    size        bigint NOT NULL CHECK (size >= 0),
    chunk_size  integer NOT NULL CHECK (chunk_size > 0),
    chunk_count integer NOT NULL CHECK (chunk_count >= 0),
    -- Concatenated 32-byte SHA-256 digests, in chunk order.
    digests     bytea NOT NULL CHECK (octet_length(digests) = chunk_count * 32),
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- Who announced holding an artifact, and until when. Announcements expire; a worker may refresh.
CREATE TABLE artifact_peers (
    hash        bytea NOT NULL REFERENCES artifacts(hash),
    worker_id   uuid NOT NULL REFERENCES workers(id),
    addr        text NOT NULL,
    announced_at timestamptz NOT NULL DEFAULT now(),
    expires_at  timestamptz NOT NULL,
    PRIMARY KEY (hash, worker_id)
);
CREATE INDEX artifact_peers_expiry_idx ON artifact_peers (expires_at);
