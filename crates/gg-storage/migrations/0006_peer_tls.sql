-- Peers are reached over TLS 1.3 with a pinned raw public key (ADR-0013). The control plane records
-- the transport key fingerprint a worker announced, so readers pin the exact key it vouched for.
ALTER TABLE artifact_peers ADD COLUMN tls_fingerprint bytea NOT NULL
    CHECK (octet_length(tls_fingerprint) = 32);
