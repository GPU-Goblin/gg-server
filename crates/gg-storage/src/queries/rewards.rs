//! Reward batches and their settlement state (ADR-0015).

use ::time::OffsetDateTime;
use gg_core::*;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::StorageError;

/// A committed batch.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct BatchRow {
    /// Row id.
    pub id: Uuid,
    /// Identifier committed in the leaves.
    pub batch_id: String,
    /// Platform network.
    pub network: String,
    /// Target chain.
    pub chain_id: String,
    /// Distributing contract.
    pub contract: String,
    /// Denomination.
    pub asset: String,
    /// Merkle root.
    pub merkle_root: Vec<u8>,
    /// Leaves committed.
    pub leaf_count: i32,
    /// Total committed, as text to keep the full 128-bit range.
    pub total_amount: String,
    /// Settlement state.
    pub state: String,
    /// Transaction hash, when submitted.
    pub tx_hash: String,
    /// Height, when observed.
    pub height: Option<i64>,
    /// Operator note.
    pub note: String,
    /// Created.
    pub created_at: OffsetDateTime,
    /// Updated.
    pub updated_at: OffsetDateTime,
    /// Finalized.
    pub finalized_at: Option<OffsetDateTime>,
}

/// One committed leaf with its proof.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct LeafRow {
    /// Batch row id.
    pub batch: Uuid,
    /// Global obligation id.
    pub reward_id: String,
    /// Committed order.
    pub position: i32,
    /// Receipt settled by this leaf.
    pub receipt_id: Uuid,
    /// Beneficiary address.
    pub beneficiary: String,
    /// Amount.
    pub amount: String,
    /// Leaf hash.
    pub leaf_hash: Vec<u8>,
    /// Proof steps.
    pub proof: serde_json::Value,
}

const BATCH_COLS: &str = "id, batch_id, network, chain_id, contract, asset, merkle_root, leaf_count, total_amount::text AS total_amount, state, tx_hash, height, note, created_at, updated_at, finalized_at";

/// Insert a batch in `prepared` state.
#[allow(clippy::too_many_arguments)]
pub async fn insert_batch(
    c: &mut PgConnection,
    id: Uuid,
    batch_id: &str,
    network: &str,
    chain_id: &str,
    contract: &str,
    asset: &str,
    merkle_root: &[u8],
    leaf_count: i32,
    total_amount: &str,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO reward_batches (id, batch_id, network, chain_id, contract, asset, merkle_root, leaf_count, total_amount, state)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::numeric, 'prepared')",
    )
    .bind(id)
    .bind(batch_id)
    .bind(network)
    .bind(chain_id)
    .bind(contract)
    .bind(asset)
    .bind(merkle_root)
    .bind(leaf_count)
    .bind(total_amount)
    .execute(c)
    .await?;
    Ok(())
}

/// Insert one committed leaf. The unique reward id refuses a second commitment of the same
/// obligation, in this batch or any other.
#[allow(clippy::too_many_arguments)]
pub async fn insert_leaf(
    c: &mut PgConnection,
    batch: Uuid,
    reward_id: &str,
    position: i32,
    receipt: Uuid,
    beneficiary: &str,
    amount: &str,
    leaf_hash: &[u8],
    proof: &serde_json::Value,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO reward_batch_leaves (batch, reward_id, position, receipt_id, beneficiary, amount, leaf_hash, proof)
         VALUES ($1, $2, $3, $4, $5, $6::numeric, $7, $8)",
    )
    .bind(batch)
    .bind(reward_id)
    .bind(position)
    .bind(receipt)
    .bind(beneficiary)
    .bind(amount)
    .bind(leaf_hash)
    .bind(proof)
    .execute(c)
    .await?;
    Ok(())
}

/// One batch by its committed identifier.
pub async fn get_batch(
    c: &mut PgConnection,
    batch_id: &str,
) -> Result<Option<BatchRow>, StorageError> {
    Ok(sqlx::query_as::<_, BatchRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {BATCH_COLS} FROM reward_batches WHERE batch_id = $1"
    )))
    .bind(batch_id)
    .fetch_optional(c)
    .await?)
}

/// Lock a batch for a settlement transition.
pub async fn lock_batch(
    c: &mut PgConnection,
    batch_id: &str,
) -> Result<Option<BatchRow>, StorageError> {
    Ok(sqlx::query_as::<_, BatchRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {BATCH_COLS} FROM reward_batches WHERE batch_id = $1 FOR UPDATE"
    )))
    .bind(batch_id)
    .fetch_optional(c)
    .await?)
}

/// Batches, newest first.
pub async fn list_batches(c: &mut PgConnection, limit: i64) -> Result<Vec<BatchRow>, StorageError> {
    Ok(sqlx::query_as::<_, BatchRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {BATCH_COLS} FROM reward_batches ORDER BY created_at DESC LIMIT $1"
    )))
    .bind(limit)
    .fetch_all(c)
    .await?)
}

/// Leaves of a batch in committed order.
pub async fn list_leaves(c: &mut PgConnection, batch: Uuid) -> Result<Vec<LeafRow>, StorageError> {
    Ok(sqlx::query_as::<_, LeafRow>(
        "SELECT batch, reward_id, position, receipt_id, beneficiary, amount::text AS amount, leaf_hash, proof
         FROM reward_batch_leaves WHERE batch = $1 ORDER BY position",
    )
    .bind(batch)
    .fetch_all(c)
    .await?)
}

/// Move a batch to a settlement state.
pub async fn set_batch_state(
    c: &mut PgConnection,
    id: Uuid,
    state: &str,
    tx_hash: &str,
    height: Option<i64>,
    note: &str,
) -> Result<(), StorageError> {
    sqlx::query(
        "UPDATE reward_batches SET state = $2, tx_hash = $3, height = $4, note = $5,
         updated_at = now(), finalized_at = CASE WHEN $2 = 'finalized' THEN now() ELSE finalized_at END
         WHERE id = $1",
    )
    .bind(id)
    .bind(state)
    .bind(tx_hash)
    .bind(height)
    .bind(note)
    .execute(c)
    .await?;
    Ok(())
}

/// Receipts that earned credits and are not committed to any batch yet.
pub async fn unbatched_receipts(
    c: &mut PgConnection,
    limit: i64,
) -> Result<Vec<crate::models::ReceiptRow>, StorageError> {
    Ok(sqlx::query_as::<_, crate::models::ReceiptRow>(
        "SELECT r.id, r.attempt_id, r.worker_id, r.project_id, r.role, r.amount, r.reward_state,
                r.ledger_txn_id, r.receipt_hash, r.receipt_cbor, r.created_at
         FROM receipts r
         LEFT JOIN reward_batch_leaves l ON l.receipt_id = r.id
         WHERE l.receipt_id IS NULL AND r.amount > 0
         ORDER BY r.created_at LIMIT $1",
    )
    .bind(limit)
    .fetch_all(c)
    .await?)
}

/// Mark receipts as committed to a batch (`batched`) or settled on chain (`settled`).
pub async fn set_receipts_reward_state(
    c: &mut PgConnection,
    batch: Uuid,
    state: &str,
) -> Result<u64, StorageError> {
    Ok(sqlx::query(
        "UPDATE receipts SET reward_state = $2 WHERE id IN
         (SELECT receipt_id FROM reward_batch_leaves WHERE batch = $1)",
    )
    .bind(batch)
    .bind(state)
    .execute(c)
    .await?
    .rows_affected())
}

/// Payout address of a worker, if it declared one.
pub async fn worker_payout_address(
    c: &mut PgConnection,
    worker: WorkerId,
) -> Result<Option<String>, StorageError> {
    Ok(
        sqlx::query_as::<_, (Option<String>,)>("SELECT payout_address FROM workers WHERE id = $1")
            .bind(worker.0)
            .fetch_optional(c)
            .await?
            .and_then(|(a,)| a),
    )
}
