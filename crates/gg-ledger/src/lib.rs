//! Budgets and the double-entry ledger (ADR-0006).
//!
//! Accounts (balance = credits − debits):
//! - `project:<id>:available` and `project:<id>:reserved` — the project's budget states;
//! - `worker:<id>:earned` — credits attributed to a worker for accepted contributions;
//! - `system:grants` — source of granted credits (negative balance = issued);
//! - `system:verification` — sink of verification costs.
//!
//! Transfers:
//! - grant: `system:grants` → `available`
//! - reserve: `available` → `reserved`
//! - release: `reserved` → `available`
//! - commit+reward: `reserved` → `worker earned` + `system:verification` (+ remainder → `available`)
//!
//! Every function runs inside the caller's transaction and writes balanced entries; the database
//! trigger refuses unbalanced transactions and any mutation of history. Materialized balances in
//! `project_budgets` (`available`, `reserved`, `spent`) are updated in the same transaction and are
//! protected by CHECK constraints. Corrections are new compensating entries, never edits.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use gg_core::*;
use gg_storage::PgConnection;
use gg_storage::StorageError;
use gg_storage::queries::{ledger as q, tasks as tq};

/// Ledger errors.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// Not enough available credits.
    #[error("insufficient budget: available {available}, required {required}")]
    Insufficient {
        /// Available.
        available: u64,
        /// Required.
        required: u64,
    },
    /// Arithmetic overflow or underflow.
    #[error("credit arithmetic: {0}")]
    Credits(#[from] CreditsError),
    /// Storage.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// Project has no budget row.
    #[error("unknown project budget")]
    UnknownProject,
    /// Reservation not in the expected state.
    #[error("reservation is not reserved (state {0})")]
    ReservationState(String),
}

/// Account name helpers.
pub mod accounts {
    use gg_core::{ProjectId, WorkerId};

    /// Available budget of a project.
    pub fn available(p: ProjectId) -> String {
        format!("project:{p}:available")
    }
    /// Reserved budget of a project.
    pub fn reserved(p: ProjectId) -> String {
        format!("project:{p}:reserved")
    }
    /// Earned credits of a worker.
    pub fn earned(w: WorkerId) -> String {
        format!("worker:{w}:earned")
    }
    /// Source of grants.
    pub const GRANTS: &str = "system:grants";
    /// Verification cost sink.
    pub const VERIFICATION: &str = "system:verification";
}

/// Reward key for an attempt/role: the global identifier of a reward obligation.
pub fn reward_key(attempt: AttemptId, role: AttemptRole) -> String {
    format!("reward:{attempt}:{}", role.as_str())
}

/// Grant credits to a project (operator action): `system:grants` → `available`.
pub async fn grant(
    c: &mut PgConnection,
    project: ProjectId,
    amount: Credits,
    reference: &str,
) -> Result<LedgerTxnId, LedgerError> {
    q::lock_budget(c, project)
        .await?
        .ok_or(LedgerError::UnknownProject)?;
    let txn = LedgerTxnId::new();
    q::insert_ledger_txn(c, txn, "grant", None, reference).await?;
    q::insert_ledger_entry(c, txn, accounts::GRANTS, amount.get(), 0).await?;
    q::insert_ledger_entry(c, txn, &accounts::available(project), 0, amount.get()).await?;
    q::apply_budget_delta(c, project, amount.get() as i64, 0, 0).await?;
    Ok(txn)
}

/// Reserve credits for an attempt: `available` → `reserved`. Fails without writing anything if the
/// available balance is insufficient. The budget row is locked for the rest of the transaction.
pub async fn reserve(
    c: &mut PgConnection,
    project: ProjectId,
    task: TaskId,
    attempt: AttemptId,
    amount: Credits,
) -> Result<ReservationId, LedgerError> {
    let budget = q::lock_budget(c, project)
        .await?
        .ok_or(LedgerError::UnknownProject)?;
    let available = budget.available.max(0) as u64;
    if available < amount.get() {
        return Err(LedgerError::Insufficient {
            available,
            required: amount.get(),
        });
    }
    let reservation = ReservationId::new();
    tq::insert_reservation(c, reservation, project, task, attempt, amount).await?;
    let txn = LedgerTxnId::new();
    q::insert_ledger_txn(c, txn, "reserve", None, &attempt.to_string()).await?;
    q::insert_ledger_entry(c, txn, &accounts::available(project), amount.get(), 0).await?;
    q::insert_ledger_entry(c, txn, &accounts::reserved(project), 0, amount.get()).await?;
    q::apply_budget_delta(c, project, -(amount.get() as i64), amount.get() as i64, 0).await?;
    Ok(reservation)
}

/// Release a reservation: `reserved` → `available` (lease lost, rejected, canceled). Idempotent:
/// returns `Ok(false)` without writing when the reservation is not `reserved` any more.
pub async fn release(c: &mut PgConnection, attempt: AttemptId) -> Result<bool, LedgerError> {
    let Some(r) = tq::lock_reservation(c, attempt).await? else {
        return Ok(false);
    };
    if r.state != "reserved" {
        return Ok(false);
    }
    let project = ProjectId(r.project_id);
    q::lock_budget(c, project)
        .await?
        .ok_or(LedgerError::UnknownProject)?;
    let amount = r.amount.max(0) as u64;
    tq::resolve_reservation(c, ReservationId(r.id), "released").await?;
    let txn = LedgerTxnId::new();
    q::insert_ledger_txn(c, txn, "release", None, &attempt.to_string()).await?;
    q::insert_ledger_entry(c, txn, &accounts::reserved(project), amount, 0).await?;
    q::insert_ledger_entry(c, txn, &accounts::available(project), 0, amount).await?;
    q::apply_budget_delta(c, project, amount as i64, -(amount as i64), 0).await?;
    Ok(true)
}

/// Result of a reward posting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewardPosting {
    /// Ledger transaction carrying the reward.
    pub txn: LedgerTxnId,
    /// Whether this call created it (`false` when the reward key already existed).
    pub created: bool,
}

/// Commit the reservation of an accepted attempt and post the reward:
/// `reserved` → `worker earned` (reward) + `system:verification` (cost) + `available` (remainder).
///
/// Exactly one effective write per `reward_key` (invariant I5): if the key already exists, nothing
/// is written and the existing transaction id is returned with `created: false`.
pub async fn commit_and_reward(
    c: &mut PgConnection,
    attempt: AttemptId,
    worker: WorkerId,
    reward: Credits,
    verification_cost: Credits,
    reward_key: &str,
) -> Result<RewardPosting, LedgerError> {
    if let Some(existing) = q::find_txn_by_reward_key(c, reward_key).await? {
        return Ok(RewardPosting {
            txn: existing,
            created: false,
        });
    }
    let r = tq::lock_reservation(c, attempt)
        .await?
        .ok_or(StorageError::NotFound("reservation"))?;
    if r.state != "reserved" {
        return Err(LedgerError::ReservationState(r.state));
    }
    let project = ProjectId(r.project_id);
    q::lock_budget(c, project)
        .await?
        .ok_or(LedgerError::UnknownProject)?;
    let reserved = Credits(r.amount.max(0) as u64);
    let spent = reward.checked_add(verification_cost)?;
    let remainder = reserved.checked_sub(spent)?;
    tq::resolve_reservation(c, ReservationId(r.id), "committed").await?;
    let txn = LedgerTxnId::new();
    q::insert_ledger_txn(c, txn, "reward", Some(reward_key), &attempt.to_string()).await?;
    q::insert_ledger_entry(c, txn, &accounts::reserved(project), reserved.get(), 0).await?;
    if !reward.is_zero() {
        q::insert_ledger_entry(c, txn, &accounts::earned(worker), 0, reward.get()).await?;
    }
    if !verification_cost.is_zero() {
        q::insert_ledger_entry(c, txn, accounts::VERIFICATION, 0, verification_cost.get()).await?;
    }
    if !remainder.is_zero() {
        q::insert_ledger_entry(c, txn, &accounts::available(project), 0, remainder.get()).await?;
    }
    q::apply_budget_delta(
        c,
        project,
        remainder.get() as i64,
        -(reserved.get() as i64),
        spent.get() as i64,
    )
    .await?;
    Ok(RewardPosting { txn, created: true })
}

/// Balance snapshot of a project (materialized) for API responses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BudgetSnapshot {
    /// Available.
    pub available: Credits,
    /// Reserved.
    pub reserved: Credits,
    /// Spent.
    pub spent: Credits,
}

/// Read a project's balances.
pub async fn budget(
    c: &mut PgConnection,
    project: ProjectId,
) -> Result<BudgetSnapshot, LedgerError> {
    let b = q::get_budget(c, project)
        .await?
        .ok_or(LedgerError::UnknownProject)?;
    Ok(BudgetSnapshot {
        available: Credits(b.available.max(0) as u64),
        reserved: Credits(b.reserved.max(0) as u64),
        spent: Credits(b.spent.max(0) as u64),
    })
}

/// Earned balance of a worker from the ledger.
pub async fn worker_earned(c: &mut PgConnection, worker: WorkerId) -> Result<Credits, LedgerError> {
    Ok(Credits(
        q::account_balance(c, &accounts::earned(worker))
            .await?
            .max(0) as u64,
    ))
}
