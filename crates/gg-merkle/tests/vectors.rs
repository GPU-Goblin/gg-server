//! Shared test vectors of the reward commitment (ADR-0015). The fixtures in
//! `tests/fixtures/merkle/` are read by this crate **and** by the CosmWasm contract, so a change in
//! the encoding, the ordering or the tree breaks both sides at once instead of letting them drift.
//! Regenerate deliberately with `GG_UPDATE_VECTORS=1 cargo test -p gg-merkle --test vectors`.

use std::path::PathBuf;

use gg_merkle::{Batch, MerkleError, ProofStep, RewardLeafV1, Side, verify_proof};
use serde::{Deserialize, Serialize};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/merkle")
}

#[derive(Serialize, Deserialize)]
struct Vector {
    leaves: Vec<RewardLeafV1>,
    /// Committed order (reward ids), after sorting.
    order: Vec<String>,
    /// Hex of each leaf hash, in committed order.
    leaf_hashes: Vec<String>,
    root: String,
    total_amount: String,
    /// Proof of each leaf, in committed order.
    proofs: Vec<Vec<ProofStep>>,
}

fn leaf(reward_id: &str, beneficiary: &str, amount: u128) -> RewardLeafV1 {
    RewardLeafV1 {
        network: "gg-test-vectors".into(),
        chain_id: "gg-localnet-1".into(),
        contract: "rg1contractexampleaddress000000000000000".into(),
        batch_id: "batch-0001".into(),
        reward_id: reward_id.into(),
        beneficiary: beneficiary.into(),
        asset: "utrg".into(),
        amount,
        receipt_hash: "11".repeat(32),
    }
}

fn check(name: &str, leaves: Vec<RewardLeafV1>) -> Batch {
    let batch = Batch::build(leaves.clone()).expect("batch");
    let current = Vector {
        leaves,
        order: batch.leaves().iter().map(|l| l.reward_id.clone()).collect(),
        leaf_hashes: batch
            .leaves()
            .iter()
            .map(|l| hex::encode(l.hash().unwrap()))
            .collect(),
        root: batch.root_hex(),
        total_amount: batch.total_amount().unwrap().to_string(),
        proofs: (0..batch.leaves().len())
            .map(|i| batch.proof(i).unwrap())
            .collect(),
    };
    let path = fixtures_dir().join(format!("{name}.json"));
    if std::env::var_os("GG_UPDATE_VECTORS").is_some() {
        std::fs::create_dir_all(fixtures_dir()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&current).unwrap() + "\n",
        )
        .unwrap();
        return batch;
    }
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing fixture {}: {e} (GG_UPDATE_VECTORS=1)",
            path.display()
        )
    });
    let expected: Vector = serde_json::from_str(&text).unwrap();
    assert_eq!(
        current.order, expected.order,
        "{name}: committed order changed"
    );
    assert_eq!(
        current.leaf_hashes, expected.leaf_hashes,
        "{name}: leaf encoding changed"
    );
    assert_eq!(current.root, expected.root, "{name}: root changed");
    assert_eq!(
        current.total_amount, expected.total_amount,
        "{name}: total changed"
    );
    assert_eq!(
        serde_json::to_value(&current.proofs).unwrap(),
        serde_json::to_value(&expected.proofs).unwrap(),
        "{name}: proofs changed"
    );
    batch
}

#[test]
fn one_leaf_is_its_own_root() {
    let batch = check("batch_single", vec![leaf("reward-1", "rg1alice", 10)]);
    assert_eq!(batch.root(), batch.leaves()[0].hash().unwrap());
    let proof = batch.proof(0).unwrap();
    assert!(proof.is_empty(), "a single leaf needs no sibling");
    assert!(verify_proof(&batch.leaves()[0], &proof, &batch.root()).unwrap());
}

#[test]
fn an_even_batch_commits_every_leaf() {
    let batch = check(
        "batch_even",
        vec![
            leaf("reward-2", "rg1bob", 7),
            leaf("reward-1", "rg1alice", 10),
            leaf("reward-4", "rg1dan", 1),
            leaf("reward-3", "rg1carol", 5),
        ],
    );
    // Sorted by reward id, whatever order the builder passed them in.
    assert_eq!(
        batch
            .leaves()
            .iter()
            .map(|l| l.reward_id.as_str())
            .collect::<Vec<_>>(),
        ["reward-1", "reward-2", "reward-3", "reward-4"]
    );
    assert_eq!(batch.total_amount(), Some(23));
    for (i, l) in batch.leaves().iter().enumerate() {
        let proof = batch.proof(i).unwrap();
        assert!(verify_proof(l, &proof, &batch.root()).unwrap(), "leaf {i}");
        assert_eq!(proof.len(), 2);
    }
}

#[test]
fn an_odd_leaf_is_promoted_not_duplicated() {
    let batch = check(
        "batch_odd",
        vec![
            leaf("reward-1", "rg1alice", 10),
            leaf("reward-2", "rg1bob", 7),
            leaf("reward-3", "rg1carol", 5),
        ],
    );
    for (i, l) in batch.leaves().iter().enumerate() {
        assert!(
            verify_proof(l, &batch.proof(i).unwrap(), &batch.root()).unwrap(),
            "leaf {i}"
        );
    }
    // The promoted leaf climbs one level, so its proof is shorter than the paired ones.
    assert_eq!(batch.proof(2).unwrap().len(), 1);
    assert_eq!(batch.proof(0).unwrap().len(), 2);
    // Duplicating the last leaf would be a different batch, refused as a duplicate reward id.
    let mut duplicated = batch.leaves().to_vec();
    duplicated.push(batch.leaves()[2].clone());
    assert_eq!(
        Batch::build(duplicated),
        Err(MerkleError::DuplicateRewardId("reward-3".into()))
    );
}

#[test]
fn a_proof_is_worthless_anywhere_else() {
    let batch = check(
        "batch_binding",
        vec![
            leaf("reward-1", "rg1alice", 10),
            leaf("reward-2", "rg1bob", 7),
        ],
    );
    let proof = batch.proof(0).unwrap();
    let original = &batch.leaves()[0];
    assert!(verify_proof(original, &proof, &batch.root()).unwrap());

    // Every field that decides the payment is inside the hash.
    for mutate in [
        |l: &mut RewardLeafV1| l.beneficiary = "rg1mallory".into(),
        |l: &mut RewardLeafV1| l.amount += 1,
        |l: &mut RewardLeafV1| l.chain_id = "other-chain-1".into(),
        |l: &mut RewardLeafV1| l.contract = "rg1othercontract00000000000000000000000".into(),
        |l: &mut RewardLeafV1| l.batch_id = "batch-0002".into(),
        |l: &mut RewardLeafV1| l.network = "gg-other".into(),
        |l: &mut RewardLeafV1| l.asset = "uatom".into(),
        |l: &mut RewardLeafV1| l.reward_id = "reward-9".into(),
        |l: &mut RewardLeafV1| l.receipt_hash = "22".repeat(32),
    ] {
        let mut tampered = original.clone();
        mutate(&mut tampered);
        assert!(
            !verify_proof(&tampered, &proof, &batch.root()).unwrap(),
            "a changed field must break the proof"
        );
    }

    // A sibling of the wrong side does not reconstruct the root either.
    let flipped: Vec<ProofStep> = proof
        .iter()
        .map(|s| ProofStep {
            hash: s.hash.clone(),
            side: match s.side {
                Side::Left => Side::Right,
                Side::Right => Side::Left,
            },
        })
        .collect();
    assert!(!verify_proof(original, &flipped, &batch.root()).unwrap());

    // An over-long proof is refused before any hashing work.
    let long: Vec<ProofStep> = std::iter::repeat_n(proof[0].clone(), 33).collect();
    assert_eq!(
        verify_proof(original, &long, &batch.root()),
        Err(MerkleError::ProofTooLong)
    );
}

#[test]
fn malformed_leaves_and_empty_batches_are_refused() {
    assert_eq!(Batch::build(vec![]), Err(MerkleError::Empty));
    let mut bad = leaf("reward-1", "rg1alice", 1);
    bad.beneficiary = String::new();
    assert_eq!(
        Batch::build(vec![bad]),
        Err(MerkleError::Field("beneficiary"))
    );
    let mut bad = leaf("reward-1", "rg1alice", 1);
    bad.receipt_hash = "not hex".into();
    assert_eq!(Batch::build(vec![bad]), Err(MerkleError::ReceiptHash));
    let mut bad = leaf("reward-1", "rg1alice", 1);
    bad.network = "n".repeat(257);
    assert_eq!(Batch::build(vec![bad]), Err(MerkleError::Field("network")));
    // An amount that would overflow the total is reported rather than wrapping.
    let batch = Batch::build(vec![
        leaf("reward-1", "rg1alice", u128::MAX),
        leaf("reward-2", "rg1bob", 1),
    ])
    .unwrap();
    assert_eq!(batch.total_amount(), None);
}

#[test]
fn a_larger_batch_keeps_every_proof_valid() {
    let leaves: Vec<RewardLeafV1> = (0..37)
        .map(|i| {
            leaf(
                &format!("reward-{i:04}"),
                &format!("rg1worker{i}"),
                i as u128 + 1,
            )
        })
        .collect();
    let batch = Batch::build(leaves).unwrap();
    assert_eq!(batch.total_amount(), Some((1..=37u128).sum()));
    for (i, l) in batch.leaves().iter().enumerate() {
        let proof = batch.proof(i).unwrap();
        assert!(
            proof.len() <= 6,
            "proof of depth {} for 37 leaves",
            proof.len()
        );
        assert!(verify_proof(l, &proof, &batch.root()).unwrap(), "leaf {i}");
    }
    // A leaf of another batch is not in this tree.
    let outsider = leaf("reward-9999", "rg1eve", 1);
    assert!(!verify_proof(&outsider, &batch.proof(0).unwrap(), &batch.root()).unwrap());
}
