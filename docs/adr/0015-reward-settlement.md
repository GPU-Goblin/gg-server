# ADR-0015 — Reward commitments and deferred settlement (CosmWasm)

Status: accepted and implemented (2026-09-17), phase P7. **No real funds, no mainnet.**

## Context

Internal credits (TC) already account for accepted contributions: reservation, commitment, single
reward, signed receipt. The project brief (§15) then asks for a separate settlement layer: a
commitment register distinct from distribution; batches committed by a Merkle tree with a defined
encoding, order and edge cases; test vectors shared between the backend and the contracts; leaves that
bind at least the network, chain, contract, batch, global reward identifier, beneficiary, asset, amount
and receipt; protection against replay and double claims; an explicit publishing authority; and a
separate settlement state machine. The brief forbids any mainnet and any real value.

## Decision — the commitment (`gg-merkle`)

A single place defines the commitment, used **identically** by the control plane and by the contract:

- **Explicit encoding**: `"gpu-goblin/v1\nreward-leaf\n"`, then each field prefixed with its length
  as a big-endian `u32`, in this order: network, chain, contract, batch, reward identifier,
  beneficiary, asset; then the amount as 16 big-endian bytes and the receipt hash as 32 raw bytes. No
  serde format is involved: the bytes can be recomputed in any language from this sentence alone.
- **Domain separation**: internal nodes use `"gpu-goblin/v1\nreward-node\n"`. A leaf hash therefore
  cannot be reinterpreted as a node, which rules out wrong-depth proofs.
- **Order**: leaves are sorted by reward identifier and duplicates are rejected. Two honest builders
  get the same root.
- **Odd leaf**: it is **promoted as is** to the next level, never duplicated. Duplication (the Bitcoin
  scheme) lets two different lists share a root.
- **Edge cases**: an empty batch is rejected; a single-leaf batch has that leaf's hash as its root, and
  its proof is empty.
- **Shared vectors**: `tests/fixtures/merkle/*.json` are read by `gg-merkle` **and** replayed by the
  contract (`the_shared_test_vectors_verify_on_chain`). A drift breaks a test, not a payment.

## Decision — distribution (`gg-rewards` contract, CosmWasm)

- **Explicit administered authority**: an `authority` address publishes batches, pauses the contract
  and can hand over its power; every action emits an auditable event. This authority **attests** an
  off-chain acceptance; it does not turn it into a mathematical proof.
- **Immutable root**: republishing an existing batch identifier is rejected.
- **Solvency**: at publication time, the balance held must cover **everything committed and not yet
  paid**, otherwise the batch is rejected — at publication, not at claim time.
- **Payment to the committed beneficiary**: anyone can relay the transaction; the funds go to the
  address recorded in the leaf. A relayer gains nothing.
- **One obligation, one payment**: claims are recorded under the global reward identifier, so the
  same obligation cannot be paid from two different batches.
- **Bindings checked before any proof**: contract, chain, network and denomination must match; a proof
  valid elsewhere is not valid here.
- **Bounds**: proofs are limited to 32 steps, amounts are `Uint128` with checked additions, and a batch
  cannot pay out more than its announced total.
- **Token**: a test denomination. No currency standard is rewritten; if a cw20 token is chosen later,
  it will be reused as is.

## Decision — settlement (control plane)

- A batch is built from **already accepted receipts** that belong to no batch. A uniqueness
  constraint on the reward identifier makes a second commitment impossible.
- A contribution whose contributor has **not declared a payout address** is never committed: the
  platform does not choose who gets paid on their behalf (`gg worker payout`).
- **Separate state machine**: `prepared → submitted → observed → finalized`, with `to_reconcile`
  reachable from any non-final state. A duplicated RPC event does not move the machine backwards. An
  observation must report the root **actually seen**: if it differs from the committed root, the batch
  goes to reconciliation. **A transaction identifier is never enough**: only the transition to
  `finalized` marks the contributions as settled.
- The control plane **talks to no chain**: an operator reports what was submitted, observed and
  finalized. This is a choice, not a gap: an RPC client that decided finality on its own would be
  exactly what the brief forbids.
- Internal credits do not move here: they were accounted for when the receipt was issued. The batch
  commits an **external** distribution; the receipt is the link between the two, and provenance is
  carried by the receipts, not by some supposed identity of each fraction of a token.

## What this does not do, and must not be presented as doing

- **No proof of computation.** A root proves that a list was committed. It proves neither that a
  computation took place nor that it was correct. Acceptance is off-chain, signed by validators
  (ADR-0014), and the leaf merely references the receipt that attests it.
- **No authorization to issue or market an asset.** Contract tests replace neither a security audit,
  nor defined governance, nor a legal review. None of these exists today.
- **No guarantee of value.** A token is backed neither by a quantity of FLOPS nor by a market price.
- **No deployment.** The contract is exercised on a simulated chain (`cw-multi-test`) and compiled for
  wasm32. The target chain, its versions and its governance remain to be chosen and documented before
  any deployment, including on a testnet, and that requires explicit authorization.

## Consequences

- `cargo xtask check` builds and tests the contract, then checks that the Wasm artifact exports
  `instantiate`, `execute`, `query`, `allocate`, `deallocate` and a CosmWasm interface version.
- The contract lives in its own workspace: CosmWasm requires `panic = "abort"` for wasm32, which cannot
  be set per package in the main workspace.
- Operations: `gg admin batch-create|batch-list|batch-export|batch-settle`, `gg worker payout`.
