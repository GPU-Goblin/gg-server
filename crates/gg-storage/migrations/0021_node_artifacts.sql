-- Artifacts held by a lab node (ADR-0041, A3). The server records the hash, the size and the chunk
-- digests the node declared, signs their manifest, and keeps no copy: the node serves the bytes.
ALTER TABLE artifacts ADD COLUMN held_by_node uuid REFERENCES lab_nodes(id);

-- Where a node serves what it holds, as `artifact_peers` does for workers.
CREATE TABLE node_peers (
    hash            bytea NOT NULL CHECK (octet_length(hash) = 32),
    node_id         uuid NOT NULL REFERENCES lab_nodes(id),
    addr            text NOT NULL,
    tls_fingerprint bytea NOT NULL CHECK (octet_length(tls_fingerprint) = 32),
    announced_at    timestamptz NOT NULL DEFAULT now(),
    expires_at      timestamptz NOT NULL,
    PRIMARY KEY (hash, node_id)
);
CREATE INDEX node_peers_expires_idx ON node_peers (expires_at);
