# Selected dependencies (checked on crates.io on 2026-09-16)

| Crate | Version | Role | Verification notes |
|---|---|---|---|
| Rust | 1.96.0 stable, edition 2024 | pinned toolchain (`rust-toolchain.toml`) | burn 0.21 requires ≥1.92, sqlx 0.9 ≥1.94 |
| burn | =0.21.0 | tensors, autodiff, modules, optimizers | 0.22 is a pre-release; CPU backend = `flex` (ndarray marked legacy); GPU = `wgpu` + `vulkan` |
| cubecl / wgpu | 0.10.0 / 29 (transitive) | GPU runtime | `init_setup` panics without an adapter: the worker probes the GPU in a separate process |
| sqlx | 0.9.0 | asynchronous PostgreSQL, migrations | `query*()` requires `&'static str` or `AssertSqlSafe`; tokio runtime, `tls-none` locally |
| axum / tower-http | 0.8.9 / 0.7.1 | HTTP API, the worker's chunk server (P5) | `{param}` paths (0.8); `RequestBodyLimitLayer`, `TimeoutLayer`, `TraceLayer`; the worker reuses axum rather than adding a network stack (ADR-0012) |
| rustls / tokio-rustls | 0.23.45 / 0.26.5 | TLS 1.3 between peers, RFC 7250 raw public keys (ADR-0013) | `AlwaysResolvesServerRawPublicKeys` on the holder side, `verify_tls13_signature_with_raw_key` on the reader side; explicit `aws_lc_rs` provider |
| hyper / hyper-util / http-body-util | 1.11 / 0.1.20 / 0.1.3 | encrypted peer request and TLS service of the chunk server | body bounded by `Limited` to the size of one chunk |
| aws-lc-rs | 1.18.1 | generation of the ephemeral transport key (Ed25519 PKCS#8) | already present through rustls |
| reqwest | 0.13.5 | HTTP client | rustls + platform verifier by default; `json`, `stream` |
| serde_ipld_dagcbor | 0.7.0 | strict DAG-CBOR (deterministic encoding) | sorted keys, finite 64-bit floats, strict decoding |
| serde_json | 1.0.151, feature `float_roundtrip` | API JSON | **mandatory**: without `float_roundtrip`, parsing an f64 can be off by one ulp (e.g. 0.11082745764249695) and signatures of messages carried in JSON fail; property test in `gg-protocol/tests/json_roundtrip.rs` |
| ed25519-dalek | 3.0.0 | Ed25519 signatures | `SigningKey::from_bytes`, `verify_strict`; randomness via getrandom 0.4 |
| sha2 | 0.11.0 | SHA-256 | digest 0.11 |
| uuid | 1.26.1 | UUIDv7 identifiers | sortable by time |
| time | 0.3.55 | server timestamps | protocol messages use integer `unix_millis` |
| clap / toml / tracing | 4.6 / 1.1 / 0.1 | CLI, configuration, logs | — |
| lettre | 0.11.23 | sign-in e-mails to researchers (ADR-0028) | default features off; `smtp-transport` + `tokio1-rustls` with the `aws-lc-rs` provider already used by rustls, system root certificates; no native-tls, no ring |
| proptest | 1.11.0 | property tests | — |

Policy: versions pinned by a committed `Cargo.lock`; monthly update with `cargo update` and a rerun
of the protocol test vectors; `cargo deny` to be added in P3 (license inventory).

## Libraries evaluated and rejected

| Crate | Version examined | Decision |
|---|---|---|
| librqbit | 9.0.1 (2026-09-17) | **rejected** for the P5 transport: BitTorrent v1 only (no trace of BEP 52 / v2 / merkle in the README or `src/`), hence SHA-1 chunks while our artifacts are addressed by SHA-256; bundles DHT, trackers and UPnP. See ADR-0012. |
| cratetorrent | 0.1.0 | **rejected**: "simple BitTorrent V1 engine library", unmaintained. |
