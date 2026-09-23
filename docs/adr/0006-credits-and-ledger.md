# ADR-0006 — Test credits and the accounting ledger

Status: accepted (2026-09-16)

## Decision

- Unit `TC` (test credit); amounts are integer `Credits(u64)` with no promised monetary value; serialized
  as a string in JSON.
- Accounts: `project:<id>:available`, `project:<id>:reserved`, `project:<id>:spent`,
  `worker:<id>:earned`, `system:grants`, `system:verification`. Table `ledger_transactions`
  (id, kind, nullable unique reward_key) and `ledger_entries` (transaction, account, debit/credit).
  Every transaction is balanced (sum of debits = sum of credits), checked by a deferrable trigger.
- `project_budgets` holds the materialized balances with `CHECK (available >= 0 AND reserved >= 0)`;
  every change locks the row (`FOR UPDATE`) and writes the corresponding entries.
- Rewards are fixed in advance in the task definition's `reward_schedule`; the reservation made at
  assignment covers `role reward + verification_cost`; acceptance commits it, rejection or cancellation
  releases it. Corrections are compensating entries.

## Consequences

Invariants I5/I6 can be tested with real concurrent transactions; token provenance (P7) is carried by the
receipts, not by an identity attached to each fraction.
