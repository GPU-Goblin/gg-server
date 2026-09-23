# ADR-0001 — Rust toolchain and workspace layout

Status: accepted (2026-09-16)

## Decision

Stable Rust 1.96.0 pinned in `rust-toolchain.toml`, edition 2024, `rust-version = "1.96"` (the only
version we have verified). A single Cargo workspace with a committed `Cargo.lock` and resolver 3. One crate
per responsibility (see README). The CosmWasm contracts (P7) will live in a separate `contracts/` workspace.

## Consequences

- Builds are reproducible; version upgrades require an explicit change.
- `cargo xtask` replaces shell scripts for automation.
