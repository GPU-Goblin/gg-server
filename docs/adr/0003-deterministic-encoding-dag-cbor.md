# ADR-0003 — Deterministic encoding: DAG-CBOR

Status: accepted (2026-09-16)

## Context

Signed metadata needs a canonical serialization. Plain JSON (key order, whitespace, floats) and Protobuf
(unknown fields, no guaranteed order) do not provide one.

## Options

1. DAG-CBOR (IPLD) via `serde_ipld_dagcbor` 0.7: a strict subset of RFC 8949 §4.2, keys sorted on
   encoding, finite 64-bit floats, strict decoding (rejects non-minimal, unsorted and indefinite-length items).
2. A home-grown "core deterministic" CBOR: more code to audit.
3. Canonical JSON (RFC 8785): no maintained Rust implementation with native byte strings.

## Decision

Option 1. Strict decoding is enabled (the `less-strict-decoding` feature is forbidden). The bytes produced
are checked against versioned test vectors (`tests/fixtures/protocol`) and against RFC 8949 cases
(minimal integers, key ordering).

## Consequences

- Messages contain only representable types: integers ≤ 64 bits, finite f64, text, byte strings, lists,
  maps, `null`.
- The API's JSON representations are derived from the same structs (`is_human_readable`).
- Incident in P1: verifying a decision carried as JSON failed because serde_json (without
  `float_roundtrip`) parsed 0.11082745764249695 one ulp off. Fix: the `float_roundtrip` feature is enabled
  for the whole workspace, plus a property test over random f64 values.
