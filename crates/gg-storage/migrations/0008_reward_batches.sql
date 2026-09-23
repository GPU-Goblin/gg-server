-- Deferred settlement (P7, ADR-0015). A batch commits an immutable list of rewards through a Merkle
-- root; the settlement of that batch on a chain follows its own state machine. Nothing here moves
-- internal credits: those were already accounted when the receipt was issued.

CREATE TABLE reward_batches (
    id              uuid PRIMARY KEY,
    batch_id        text NOT NULL UNIQUE,           -- identifier committed inside every leaf
    network         text NOT NULL,                  -- platform network
    chain_id        text NOT NULL,                  -- chain the payment is meant for
    contract        text NOT NULL,                  -- distributing contract address
    asset           text NOT NULL,                  -- denomination
    merkle_root     bytea NOT NULL CHECK (octet_length(merkle_root) = 32),
    leaf_count      integer NOT NULL CHECK (leaf_count > 0),
    total_amount    numeric(39, 0) NOT NULL CHECK (total_amount >= 0),
    -- prepared -> submitted -> observed -> finalized, or to_reconcile from any non-final state.
    state           text NOT NULL CHECK (state IN ('prepared','submitted','observed','finalized','to_reconcile')),
    tx_hash         text NOT NULL DEFAULT '',
    height          bigint,
    note            text NOT NULL DEFAULT '',
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    finalized_at    timestamptz
);
CREATE INDEX reward_batches_state_idx ON reward_batches (state, created_at);

CREATE TABLE reward_batch_leaves (
    batch           uuid NOT NULL REFERENCES reward_batches(id),
    -- Global obligation identifier: unique across every batch, which is what stops the same
    -- obligation being committed twice and paid twice.
    reward_id       text NOT NULL UNIQUE,
    position        integer NOT NULL,               -- committed order
    receipt_id      uuid NOT NULL REFERENCES receipts(id),
    beneficiary     text NOT NULL,
    amount          numeric(39, 0) NOT NULL CHECK (amount >= 0),
    leaf_hash       bytea NOT NULL CHECK (octet_length(leaf_hash) = 32),
    proof           jsonb NOT NULL,                 -- [{hash, side}]
    PRIMARY KEY (batch, position)
);
CREATE UNIQUE INDEX reward_batch_leaves_receipt_idx ON reward_batch_leaves (receipt_id);
