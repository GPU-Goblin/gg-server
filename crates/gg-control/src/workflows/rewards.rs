//! Deferred settlement of accepted contributions (P7, ADR-0015).
//!
//! A batch turns receipts into an immutable commitment: one leaf per obligation, a Merkle root, and
//! proofs a beneficiary can present to the distributing contract. Three things this module does not
//! do, and must not be described as doing:
//!
//! * it does not move internal credits — those were accounted when the receipt was issued;
//! * it does not prove any computation happened — the leaf references the signed receipt that
//!   attests an off-chain acceptance, and a root only proves a list was committed;
//! * it does not talk to a chain. Submission, observation and finality are recorded by an operator,
//!   because a transaction identifier alone never justifies considering a settlement done.

use gg_core::*;
use gg_merkle::{Batch, RewardLeafV1};
use gg_storage::queries::{misc, rewards, tasks};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// What an operator asks for when preparing a batch.
#[derive(Clone, Debug, Deserialize, utoipa::ToSchema)]
pub struct PrepareBatch {
    /// Identifier committed inside every leaf (unique for the contract).
    pub batch_id: String,
    /// Chain the payment is meant for.
    pub chain_id: String,
    /// Address of the distributing contract.
    pub contract: String,
    /// Denomination distributed (a test denomination in this phase).
    pub asset: String,
    /// Maximum number of receipts to commit.
    #[serde(default = "default_max_leaves")]
    pub max_leaves: u32,
    /// Show what would be committed without writing anything.
    #[serde(default)]
    pub dry_run: bool,
}
fn default_max_leaves() -> u32 {
    500
}

/// One committed obligation as exported for the contract.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct ExportedLeaf {
    /// Committed order.
    pub position: u32,
    /// The leaf itself, exactly as hashed.
    #[schema(value_type = Object)]
    pub leaf: RewardLeafV1,
    /// Proof from the leaf to the root.
    #[schema(value_type = Object)]
    pub proof: Vec<gg_merkle::ProofStep>,
    /// Receipt this reward settles.
    pub receipt_id: ReceiptId,
}

/// A prepared or settled batch.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct BatchView {
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
    /// Merkle root (hex).
    pub merkle_root: String,
    /// Leaves committed.
    pub leaf_count: u32,
    /// Total committed.
    pub total_amount: String,
    /// Settlement state.
    pub state: String,
    /// Transaction hash, once submitted.
    pub tx_hash: String,
    /// Height, once observed.
    pub height: Option<i64>,
    /// Operator note.
    pub note: String,
    /// Leaves with their proofs (only in the detailed view).
    pub leaves: Vec<ExportedLeaf>,
}

/// Settlement events an operator records. Nothing here is inferred from a chain: the control plane
/// is told what was observed, and says what that does or does not justify.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SettlementEvent {
    /// A transaction carrying the batch was sent.
    Submitted,
    /// The transaction was seen in a block, with the committed root.
    Observed,
    /// The chain's finality rule is satisfied for that block.
    Finalized,
    /// Something does not add up: the batch waits for a human.
    Reconcile,
}

/// Build the leaf of one receipt. The reward identifier is the receipt identifier: one accepted
/// contribution, one obligation, whatever batch commits it.
fn leaf_for(
    network: &str,
    chain_id: &str,
    contract: &str,
    batch_id: &str,
    asset: &str,
    receipt: &gg_storage::models::ReceiptRow,
    beneficiary: &str,
) -> RewardLeafV1 {
    RewardLeafV1 {
        network: network.to_owned(),
        chain_id: chain_id.to_owned(),
        contract: contract.to_owned(),
        batch_id: batch_id.to_owned(),
        reward_id: receipt.id.to_string(),
        beneficiary: beneficiary.to_owned(),
        asset: asset.to_owned(),
        amount: receipt.amount.max(0) as u128,
        receipt_hash: hex::encode(&receipt.receipt_hash),
    }
}

/// Prepare a batch from the accepted contributions that no batch commits yet.
pub async fn prepare(
    state: &AppState,
    actor: &str,
    req: PrepareBatch,
) -> Result<BatchView, ApiError> {
    if req.batch_id.trim().is_empty() || req.batch_id.len() > 128 {
        return Err(ApiError::Unprocessable(
            "batch_id must be 1..=128 bytes".into(),
        ));
    }
    for (name, value) in [
        ("chain_id", &req.chain_id),
        ("contract", &req.contract),
        ("asset", &req.asset),
    ] {
        if value.trim().is_empty() || value.len() > 256 {
            return Err(ApiError::Unprocessable(format!("{name} is required")));
        }
    }
    if !(1..=5000).contains(&req.max_leaves) {
        return Err(ApiError::Unprocessable(
            "max_leaves must be 1..=5000".into(),
        ));
    }
    let mut tx = state.db().begin().await?;
    if rewards::get_batch(&mut tx, &req.batch_id).await?.is_some() {
        return Err(ApiError::Conflict(format!(
            "batch {} already exists; a committed batch is never rebuilt",
            req.batch_id
        )));
    }
    let receipts = rewards::unbatched_receipts(&mut tx, req.max_leaves as i64).await?;
    if receipts.is_empty() {
        return Err(ApiError::Unprocessable(
            "no accepted contribution is waiting for settlement".into(),
        ));
    }
    let mut leaves = Vec::with_capacity(receipts.len());
    let mut skipped = 0usize;
    for r in &receipts {
        // A contributor without a payout address cannot be committed: the leaf must name who is
        // paid, and the platform never chooses that for them.
        let Some(beneficiary) =
            rewards::worker_payout_address(&mut tx, WorkerId(r.worker_id)).await?
        else {
            skipped += 1;
            continue;
        };
        leaves.push((
            r,
            leaf_for(
                state.network().as_str(),
                &req.chain_id,
                &req.contract,
                &req.batch_id,
                &req.asset,
                r,
                &beneficiary,
            ),
        ));
    }
    if leaves.is_empty() {
        return Err(ApiError::Unprocessable(format!(
            "none of the {} waiting contributions names a payout address",
            receipts.len()
        )));
    }
    let batch = Batch::build(leaves.iter().map(|(_, l)| l.clone()).collect())
        .map_err(|e| ApiError::Unprocessable(format!("commitment: {e}")))?;
    let total = batch
        .total_amount()
        .ok_or_else(|| ApiError::Unprocessable("total amount overflows".into()))?;
    let mut exported = Vec::with_capacity(batch.leaves().len());
    for (position, leaf) in batch.leaves().iter().enumerate() {
        let receipt = leaves
            .iter()
            .find(|(_, l)| l.reward_id == leaf.reward_id)
            .map(|(r, _)| *r)
            .ok_or_else(|| ApiError::Internal("committed leaf lost its receipt".into()))?;
        exported.push(ExportedLeaf {
            position: position as u32,
            leaf: leaf.clone(),
            proof: batch
                .proof(position)
                .ok_or_else(|| ApiError::Internal("proof out of range".into()))?,
            receipt_id: ReceiptId(receipt.id),
        });
    }
    let view = BatchView {
        batch_id: req.batch_id.clone(),
        network: state.network().as_str().to_owned(),
        chain_id: req.chain_id.clone(),
        contract: req.contract.clone(),
        asset: req.asset.clone(),
        merkle_root: batch.root_hex(),
        leaf_count: batch.leaves().len() as u32,
        total_amount: total.to_string(),
        state: "prepared".into(),
        tx_hash: String::new(),
        height: None,
        note: if skipped > 0 {
            format!("{skipped} contributions skipped: no payout address")
        } else {
            String::new()
        },
        leaves: exported.clone(),
    };
    if req.dry_run {
        tx.rollback().await?;
        return Ok(view);
    }
    let id = Uuid::now_v7();
    rewards::insert_batch(
        &mut tx,
        id,
        &req.batch_id,
        state.network().as_str(),
        &req.chain_id,
        &req.contract,
        &req.asset,
        &batch.root(),
        batch.leaves().len() as i32,
        &total.to_string(),
    )
    .await?;
    for e in &exported {
        let hash = e
            .leaf
            .hash()
            .map_err(|err| ApiError::Internal(err.to_string()))?;
        rewards::insert_leaf(
            &mut tx,
            id,
            &e.leaf.reward_id,
            e.position as i32,
            e.receipt_id.0,
            &e.leaf.beneficiary,
            &e.leaf.amount.to_string(),
            &hash,
            &serde_json::to_value(&e.proof).map_err(|err| ApiError::Internal(err.to_string()))?,
        )
        .await
        .map_err(|err| {
            if err.is_unique_violation() {
                ApiError::Conflict(
                    "an obligation of this batch is already committed elsewhere".into(),
                )
            } else {
                ApiError::from(err)
            }
        })?;
    }
    rewards::set_receipts_reward_state(&mut tx, id, "batched").await?;
    misc::audit(
        &mut tx,
        actor,
        "reward_batch.prepare",
        &req.batch_id,
        serde_json::json!({
            "merkle_root": batch.root_hex(),
            "leaf_count": batch.leaves().len(),
            "total_amount": total.to_string(),
            "chain_id": req.chain_id,
            "contract": req.contract,
        }),
    )
    .await?;
    misc::outbox_insert(
        &mut tx,
        "reward_batch.prepared",
        &serde_json::json!({ "batch_id": req.batch_id, "merkle_root": batch.root_hex(), "leaf_count": batch.leaves().len() }),
    )
    .await?;
    tx.commit().await?;
    tracing::info!(batch_id = %req.batch_id, root = %batch.root_hex(), leaves = batch.leaves().len(), "reward batch prepared");
    Ok(view)
}

/// Read a batch, with its leaves and proofs when `with_leaves`.
pub async fn view(
    state: &AppState,
    batch_id: &str,
    with_leaves: bool,
) -> Result<BatchView, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let b = rewards::get_batch(&mut c, batch_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("reward batch".into()))?;
    let mut leaves = Vec::new();
    if with_leaves {
        for row in rewards::list_leaves(&mut c, b.id).await? {
            let receipt = tasks::get_receipt_by_id(&mut c, ReceiptId(row.receipt_id))
                .await?
                .ok_or_else(|| ApiError::Internal("committed receipt missing".into()))?;
            leaves.push(ExportedLeaf {
                position: row.position as u32,
                leaf: RewardLeafV1 {
                    network: b.network.clone(),
                    chain_id: b.chain_id.clone(),
                    contract: b.contract.clone(),
                    batch_id: b.batch_id.clone(),
                    reward_id: row.reward_id.clone(),
                    beneficiary: row.beneficiary.clone(),
                    asset: b.asset.clone(),
                    amount: row
                        .amount
                        .parse()
                        .map_err(|_| ApiError::Internal("stored amount".into()))?,
                    receipt_hash: hex::encode(&receipt.receipt_hash),
                },
                proof: serde_json::from_value(row.proof)
                    .map_err(|e| ApiError::Internal(e.to_string()))?,
                receipt_id: ReceiptId(row.receipt_id),
            });
        }
    }
    Ok(BatchView {
        batch_id: b.batch_id,
        network: b.network,
        chain_id: b.chain_id,
        contract: b.contract,
        asset: b.asset,
        merkle_root: hex::encode(&b.merkle_root),
        leaf_count: b.leaf_count as u32,
        total_amount: b.total_amount,
        state: b.state,
        tx_hash: b.tx_hash,
        height: b.height,
        note: b.note,
        leaves,
    })
}

/// Every batch, newest first.
pub async fn list(state: &AppState) -> Result<Vec<BatchView>, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(rewards::list_batches(&mut c, 200)
        .await?
        .into_iter()
        .map(|b| BatchView {
            batch_id: b.batch_id,
            network: b.network,
            chain_id: b.chain_id,
            contract: b.contract,
            asset: b.asset,
            merkle_root: hex::encode(&b.merkle_root),
            leaf_count: b.leaf_count as u32,
            total_amount: b.total_amount,
            state: b.state,
            tx_hash: b.tx_hash,
            height: b.height,
            note: b.note,
            leaves: Vec::new(),
        })
        .collect())
}

/// Which transitions the settlement machine allows, and what each one means.
fn next_state(current: &str, event: SettlementEvent) -> Result<&'static str, ApiError> {
    let next =
        match (current, event) {
            // A resent transaction, or a duplicated RPC event, must not move the machine backwards.
            ("prepared", SettlementEvent::Submitted)
            | ("submitted", SettlementEvent::Submitted) => "submitted",
            ("submitted", SettlementEvent::Observed) | ("observed", SettlementEvent::Observed) => {
                "observed"
            }
            // Finality is asserted on an observation, never on a transaction identifier alone.
            ("observed", SettlementEvent::Finalized)
            | ("finalized", SettlementEvent::Finalized) => "finalized",
            (_, SettlementEvent::Reconcile) if current != "finalized" => "to_reconcile",
            ("to_reconcile", SettlementEvent::Submitted) => "submitted",
            (from, e) => {
                return Err(ApiError::Conflict(format!(
                    "a batch in state {from} does not accept the event {}",
                    serde_json::to_string(&e)
                        .unwrap_or_default()
                        .trim_matches('"')
                )));
            }
        };
    Ok(next)
}

/// Record a settlement event.
#[allow(clippy::too_many_arguments)]
pub async fn settle(
    state: &AppState,
    actor: &str,
    batch_id: &str,
    event: SettlementEvent,
    tx_hash: &str,
    height: Option<i64>,
    observed_root: Option<&str>,
    note: &str,
) -> Result<BatchView, ApiError> {
    if tx_hash.len() > 128 || note.len() > 1000 {
        return Err(ApiError::Unprocessable("tx_hash or note too long".into()));
    }
    let mut tx = state.db().begin().await?;
    let b = rewards::lock_batch(&mut tx, batch_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("reward batch".into()))?;
    let mut next = next_state(&b.state, event)?;
    let mut note = note.to_owned();
    if event == SettlementEvent::Observed {
        // What was seen on chain must be the root that was committed here. Anything else is a
        // reconciliation case, not a settlement.
        let seen = observed_root.unwrap_or_default();
        if seen.is_empty() {
            return Err(ApiError::Unprocessable(
                "an observation reports the root seen on chain".into(),
            ));
        }
        if seen != hex::encode(&b.merkle_root) {
            next = "to_reconcile";
            note = format!(
                "observed root {seen} differs from the committed root {}",
                hex::encode(&b.merkle_root)
            );
            tracing::warn!(batch_id, %seen, "observed root differs from the committed one");
        }
    }
    if event == SettlementEvent::Submitted && tx_hash.is_empty() {
        return Err(ApiError::Unprocessable(
            "a submission reports its transaction hash".into(),
        ));
    }
    let tx_hash = if tx_hash.is_empty() {
        b.tx_hash.clone()
    } else {
        tx_hash.to_owned()
    };
    rewards::set_batch_state(&mut tx, b.id, next, &tx_hash, height.or(b.height), &note).await?;
    if next == "finalized" {
        // Only finality marks the contributions as settled on chain.
        rewards::set_receipts_reward_state(&mut tx, b.id, "settled").await?;
    }
    misc::audit(
        &mut tx,
        actor,
        "reward_batch.settle",
        batch_id,
        serde_json::json!({ "event": event, "state": next, "tx_hash": tx_hash, "height": height, "note": note }),
    )
    .await?;
    misc::outbox_insert(
        &mut tx,
        "reward_batch.settlement",
        &serde_json::json!({ "batch_id": batch_id, "state": next, "event": event }),
    )
    .await?;
    tx.commit().await?;
    tracing::info!(batch_id, state = next, ?event, "settlement recorded");
    view(state, batch_id, false).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_settlement_machine_absorbs_duplicates_and_refuses_shortcuts() {
        use SettlementEvent::*;
        assert_eq!(next_state("prepared", Submitted).unwrap(), "submitted");
        // A duplicated RPC event changes nothing.
        assert_eq!(next_state("submitted", Submitted).unwrap(), "submitted");
        assert_eq!(next_state("submitted", Observed).unwrap(), "observed");
        assert_eq!(next_state("observed", Observed).unwrap(), "observed");
        assert_eq!(next_state("observed", Finalized).unwrap(), "finalized");
        assert_eq!(next_state("finalized", Finalized).unwrap(), "finalized");
        // A transaction identifier is not an observation, and an observation is not finality.
        assert!(next_state("prepared", Observed).is_err());
        assert!(next_state("prepared", Finalized).is_err());
        assert!(next_state("submitted", Finalized).is_err());
        // A finalized batch is never reopened.
        assert!(next_state("finalized", Reconcile).is_err());
        assert!(next_state("finalized", Submitted).is_err());
        // Anything unresolved can be sent to reconciliation, and resubmitted afterwards.
        assert_eq!(next_state("submitted", Reconcile).unwrap(), "to_reconcile");
        assert_eq!(next_state("observed", Reconcile).unwrap(), "to_reconcile");
        assert_eq!(next_state("to_reconcile", Submitted).unwrap(), "submitted");
    }
}
