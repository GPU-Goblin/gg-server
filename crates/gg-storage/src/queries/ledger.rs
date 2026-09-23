//! Raw ledger and budget rows. Balanced transactions are composed by `gg-ledger`.

use gg_core::*;
use sqlx::PgConnection;

use crate::StorageError;
use crate::models::BudgetRow;

/// Lock and read a project's budget row.
pub async fn lock_budget(
    c: &mut PgConnection,
    project: ProjectId,
) -> Result<Option<BudgetRow>, StorageError> {
    Ok(sqlx::query_as::<_, BudgetRow>(
        "SELECT project_id, available, reserved, spent FROM project_budgets WHERE project_id = $1 FOR UPDATE",
    )
    .bind(project.0)
    .fetch_optional(c)
    .await?)
}

/// Read a project's budget row without locking.
pub async fn get_budget(
    c: &mut PgConnection,
    project: ProjectId,
) -> Result<Option<BudgetRow>, StorageError> {
    Ok(sqlx::query_as::<_, BudgetRow>(
        "SELECT project_id, available, reserved, spent FROM project_budgets WHERE project_id = $1",
    )
    .bind(project.0)
    .fetch_optional(c)
    .await?)
}

/// Apply deltas to the materialized balances. CHECK constraints refuse negative results.
pub async fn apply_budget_delta(
    c: &mut PgConnection,
    project: ProjectId,
    d_available: i64,
    d_reserved: i64,
    d_spent: i64,
) -> Result<(), StorageError> {
    let r = sqlx::query(
        "UPDATE project_budgets SET available = available + $2, reserved = reserved + $3, spent = spent + $4, updated_at = now()
         WHERE project_id = $1",
    )
    .bind(project.0)
    .bind(d_available)
    .bind(d_reserved)
    .bind(d_spent)
    .execute(c)
    .await?;
    if r.rows_affected() != 1 {
        return Err(StorageError::NotFound("project budget"));
    }
    Ok(())
}

/// Insert a ledger transaction header. `reward_key` must be unique (I5).
pub async fn insert_ledger_txn(
    c: &mut PgConnection,
    id: LedgerTxnId,
    kind: &str,
    reward_key: Option<&str>,
    reference: &str,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO ledger_transactions (id, kind, reward_key, reference) VALUES ($1, $2, $3, $4)",
    )
    .bind(id.0)
    .bind(kind)
    .bind(reward_key)
    .bind(reference)
    .execute(c)
    .await?;
    Ok(())
}

/// Insert one ledger entry.
pub async fn insert_ledger_entry(
    c: &mut PgConnection,
    txn: LedgerTxnId,
    account: &str,
    debit: u64,
    credit: u64,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO ledger_entries (txn_id, account, debit, credit) VALUES ($1, $2, $3, $4)",
    )
    .bind(txn.0)
    .bind(account)
    .bind(debit as i64)
    .bind(credit as i64)
    .execute(c)
    .await?;
    Ok(())
}

/// Look up a ledger transaction by reward key.
pub async fn find_txn_by_reward_key(
    c: &mut PgConnection,
    reward_key: &str,
) -> Result<Option<LedgerTxnId>, StorageError> {
    let r: Option<(uuid::Uuid,)> =
        sqlx::query_as("SELECT id FROM ledger_transactions WHERE reward_key = $1")
            .bind(reward_key)
            .fetch_optional(c)
            .await?;
    Ok(r.map(|(u,)| LedgerTxnId(u)))
}

/// Net balance of an account: sum(credit) - sum(debit).
pub async fn account_balance(c: &mut PgConnection, account: &str) -> Result<i64, StorageError> {
    let (bal,): (i64,) = sqlx::query_as(
        "SELECT coalesce(sum(credit) - sum(debit), 0)::bigint FROM ledger_entries WHERE account = $1",
    )
    .bind(account)
    .fetch_one(c)
    .await?;
    Ok(bal)
}

/// Global check: every transaction balances (returns the ids of unbalanced ones, expected empty).
pub async fn unbalanced_transactions(
    c: &mut PgConnection,
) -> Result<Vec<uuid::Uuid>, StorageError> {
    let rows: Vec<(uuid::Uuid,)> = sqlx::query_as(
        "SELECT txn_id FROM ledger_entries GROUP BY txn_id HAVING sum(debit) <> sum(credit)",
    )
    .fetch_all(c)
    .await?;
    Ok(rows.into_iter().map(|(u,)| u).collect())
}

/// Ledger entries of an account (most recent first).
pub async fn list_entries(
    c: &mut PgConnection,
    account: &str,
    limit: i64,
) -> Result<Vec<(uuid::Uuid, String, i64, i64)>, StorageError> {
    Ok(sqlx::query_as(
        "SELECT e.txn_id, t.kind, e.debit, e.credit FROM ledger_entries e JOIN ledger_transactions t ON t.id = e.txn_id
         WHERE e.account = $1 ORDER BY e.id DESC LIMIT $2",
    )
    .bind(account)
    .bind(limit)
    .fetch_all(c)
    .await?)
}
