# ADR-0007 — Content-addressed artifacts and the GGT1 tensor format

Status: accepted (2026-09-16)

## Decision

- Durable local storage: `objects/sha256/aa/bb/<hex>`, written to a temporary file, then fsync, then
  rename; the `artifacts` row is inserted only afterwards. Uploads are streamed with a size limit declared
  up front; the hash is computed during reception and compared with the announced hash.
- The `GGT1` tensor format (minimal header, little-endian f32) rather than JSON or an external format
  (safetensors remains an option for later export): a bounded parser, exact size checks, and rejection of
  non-finite values when a tensor enters the catalog.
- An `ArtifactTransport` interface (HTTP in P1, P2P in P5): artifacts are immutable, which makes transports
  interchangeable.
