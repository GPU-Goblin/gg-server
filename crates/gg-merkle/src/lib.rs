//! Reward batch commitments (ADR-0015).
//!
//! A batch commits an immutable list of rewards through a Merkle tree. This crate holds the only
//! definition of that commitment: the leaf encoding, the ordering, the tree, the proofs and their
//! verification. The control plane and the CosmWasm contract both use it, so a proof accepted
//! off-chain is the same object as a proof accepted on-chain, and the shared test vectors in
//! `tests/fixtures/merkle` pin it.
//!
//! Deliberate choices, all load-bearing:
//!
//! * **Explicit encoding**: every field is length-prefixed and the whole is domain-tagged. No serde
//!   format is involved, so the bytes can be recomputed in any language from the specification.
//! * **Leaves and nodes live in different domains**: a leaf hash can never be read as an inner node,
//!   which is what makes a forged proof of the wrong depth useless.
//! * **An odd node is promoted unchanged** instead of being duplicated. Duplicating the last node
//!   (the Bitcoin scheme) makes two different leaf lists share a root; promoting does not.
//! * **Leaves are sorted by reward id and duplicates are refused**, so the same list always yields
//!   the same root, whoever builds it.
//!
//! What a root proves: that a list of rewards was committed. Nothing else. It does not prove the
//! work was done, nor that anyone accepted it. That acceptance is off-chain, signed, and attested by
//! the receipts the leaves reference.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Domain tag of a leaf.
pub const LEAF_TAG: &[u8] = b"gpu-goblin/v1\nreward-leaf\n";
/// Domain tag of an inner node.
pub const NODE_TAG: &[u8] = b"gpu-goblin/v1\nreward-node\n";
/// Longest proof a verifier accepts (2^32 leaves); bounds the work a claim can cost.
pub const MAX_PROOF_LEN: usize = 32;
/// Longest text accepted in a field, so a leaf never grows unbounded.
pub const MAX_FIELD_LEN: usize = 256;

/// A 32-byte hash.
pub type Hash = [u8; 32];

/// One committed reward. Every field that decides who gets paid, on which chain and for what, is
/// inside the hash: a proof valid for one contract, chain, batch or beneficiary is valid for no
/// other.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewardLeafV1 {
    /// Network identifier of the platform (`gg-dev-local`, …).
    pub network: String,
    /// Chain the payment happens on.
    pub chain_id: String,
    /// Address of the distributing contract.
    pub contract: String,
    /// Batch this leaf belongs to.
    pub batch_id: String,
    /// Globally unique obligation identifier. The same obligation cannot be paid twice, even from
    /// two different batches, because claims are recorded under this id.
    pub reward_id: String,
    /// Who is paid. A third party may relay the transaction; the funds go here.
    pub beneficiary: String,
    /// Asset denomination.
    pub asset: String,
    /// Amount in the smallest unit.
    pub amount: u128,
    /// Hash of the contribution receipt this reward settles (hex, 32 bytes).
    pub receipt_hash: String,
}

/// Why a leaf or a batch was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MerkleError {
    /// A text field is empty or too long.
    Field(&'static str),
    /// `receipt_hash` is not 32 bytes of hex.
    ReceiptHash,
    /// The batch has no leaf.
    Empty,
    /// Two leaves share a reward id.
    DuplicateRewardId(String),
    /// The proof is longer than [`MAX_PROOF_LEN`].
    ProofTooLong,
}

impl core::fmt::Display for MerkleError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MerkleError::Field(name) => write!(f, "field {name} is empty or too long"),
            MerkleError::ReceiptHash => write!(f, "receipt_hash must be 32 bytes of hex"),
            MerkleError::Empty => write!(f, "a batch commits at least one reward"),
            MerkleError::DuplicateRewardId(id) => write!(f, "reward id {id} appears twice"),
            MerkleError::ProofTooLong => write!(f, "proof longer than {MAX_PROOF_LEN} nodes"),
        }
    }
}

fn check_field(name: &'static str, value: &str) -> Result<(), MerkleError> {
    if value.is_empty() || value.len() > MAX_FIELD_LEN {
        return Err(MerkleError::Field(name));
    }
    Ok(())
}

fn push_field(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value);
}

impl RewardLeafV1 {
    /// Structural validation. A leaf that does not pass is never committed.
    pub fn validate(&self) -> Result<(), MerkleError> {
        check_field("network", &self.network)?;
        check_field("chain_id", &self.chain_id)?;
        check_field("contract", &self.contract)?;
        check_field("batch_id", &self.batch_id)?;
        check_field("reward_id", &self.reward_id)?;
        check_field("beneficiary", &self.beneficiary)?;
        check_field("asset", &self.asset)?;
        if hex::decode(&self.receipt_hash).map(|b| b.len()) != Ok(32) {
            return Err(MerkleError::ReceiptHash);
        }
        Ok(())
    }

    /// Canonical bytes: a domain tag then every field, each preceded by its length in big-endian
    /// u32, in this exact order. The amount is 16 big-endian bytes, the receipt hash 32 raw bytes.
    pub fn encode(&self) -> Result<Vec<u8>, MerkleError> {
        self.validate()?;
        let mut out = Vec::with_capacity(LEAF_TAG.len() + 256);
        out.extend_from_slice(LEAF_TAG);
        push_field(&mut out, self.network.as_bytes());
        push_field(&mut out, self.chain_id.as_bytes());
        push_field(&mut out, self.contract.as_bytes());
        push_field(&mut out, self.batch_id.as_bytes());
        push_field(&mut out, self.reward_id.as_bytes());
        push_field(&mut out, self.beneficiary.as_bytes());
        push_field(&mut out, self.asset.as_bytes());
        out.extend_from_slice(&self.amount.to_be_bytes());
        let receipt = hex::decode(&self.receipt_hash).map_err(|_| MerkleError::ReceiptHash)?;
        out.extend_from_slice(&receipt);
        Ok(out)
    }

    /// Hash of the leaf.
    pub fn hash(&self) -> Result<Hash, MerkleError> {
        Ok(sha256(&self.encode()?))
    }
}

fn sha256(bytes: &[u8]) -> Hash {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

/// Hash of an inner node, in its own domain.
pub fn node_hash(left: &Hash, right: &Hash) -> Hash {
    let mut h = Sha256::new();
    h.update(NODE_TAG);
    h.update(left);
    h.update(right);
    h.finalize().into()
}

/// Side a sibling sits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The sibling is the left child; the running hash is the right one.
    Left,
    /// The sibling is the right child.
    Right,
}

/// One step of a proof.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofStep {
    /// Sibling hash (hex).
    pub hash: String,
    /// Which side the sibling is on.
    pub side: Side,
}

/// A committed batch: its ordered leaves and the resulting root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch {
    leaves: Vec<RewardLeafV1>,
    hashes: Vec<Hash>,
    root: Hash,
}

impl Batch {
    /// Build a batch: validate every leaf, refuse duplicate reward ids, sort by reward id, then
    /// build the tree. The order is imposed here so that two honest builders always agree.
    pub fn build(mut leaves: Vec<RewardLeafV1>) -> Result<Self, MerkleError> {
        if leaves.is_empty() {
            return Err(MerkleError::Empty);
        }
        for leaf in &leaves {
            leaf.validate()?;
        }
        leaves.sort_by(|a, b| a.reward_id.cmp(&b.reward_id));
        for pair in leaves.windows(2) {
            if pair[0].reward_id == pair[1].reward_id {
                return Err(MerkleError::DuplicateRewardId(pair[0].reward_id.clone()));
            }
        }
        let hashes = leaves
            .iter()
            .map(|l| l.hash())
            .collect::<Result<Vec<_>, _>>()?;
        let root = root_of(&hashes);
        Ok(Self {
            leaves,
            hashes,
            root,
        })
    }

    /// Committed root.
    pub fn root(&self) -> Hash {
        self.root
    }

    /// Root as hex.
    pub fn root_hex(&self) -> String {
        hex::encode(self.root)
    }

    /// Leaves in committed order.
    pub fn leaves(&self) -> &[RewardLeafV1] {
        &self.leaves
    }

    /// Total committed amount, or `None` on overflow (a batch that cannot be paid is never built).
    pub fn total_amount(&self) -> Option<u128> {
        self.leaves
            .iter()
            .try_fold(0u128, |acc, l| acc.checked_add(l.amount))
    }

    /// Position of a reward id in the committed order.
    pub fn index_of(&self, reward_id: &str) -> Option<usize> {
        self.leaves.iter().position(|l| l.reward_id == reward_id)
    }

    /// Proof for the leaf at `index`.
    pub fn proof(&self, index: usize) -> Option<Vec<ProofStep>> {
        if index >= self.hashes.len() {
            return None;
        }
        let mut proof = Vec::new();
        let mut level = self.hashes.clone();
        let mut position = index;
        while level.len() > 1 {
            let mut next = Vec::with_capacity(level.len().div_ceil(2));
            let mut i = 0;
            while i + 1 < level.len() {
                if position == i {
                    proof.push(ProofStep {
                        hash: hex::encode(level[i + 1]),
                        side: Side::Right,
                    });
                } else if position == i + 1 {
                    proof.push(ProofStep {
                        hash: hex::encode(level[i]),
                        side: Side::Left,
                    });
                }
                next.push(node_hash(&level[i], &level[i + 1]));
                i += 2;
            }
            if i < level.len() {
                // Odd node: promoted unchanged, so it contributes no proof step.
                next.push(level[i]);
            }
            position /= 2;
            level = next;
        }
        Some(proof)
    }
}

/// Root of a level of hashes. One hash is its own root; an odd node is promoted unchanged.
pub fn root_of(hashes: &[Hash]) -> Hash {
    let mut level = hashes.to_vec();
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        let mut i = 0;
        while i + 1 < level.len() {
            next.push(node_hash(&level[i], &level[i + 1]));
            i += 2;
        }
        if i < level.len() {
            next.push(level[i]);
        }
        level = next;
    }
    level.first().copied().unwrap_or([0u8; 32])
}

/// Recompute a root from a leaf and its proof. This is the only verification a payer needs, and it
/// is the same code on both sides.
pub fn verify_proof(
    leaf: &RewardLeafV1,
    proof: &[ProofStep],
    root: &Hash,
) -> Result<bool, MerkleError> {
    if proof.len() > MAX_PROOF_LEN {
        return Err(MerkleError::ProofTooLong);
    }
    let mut current = leaf.hash()?;
    for step in proof {
        let sibling = hex::decode(&step.hash)
            .ok()
            .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
            .ok_or(MerkleError::Field("proof step"))?;
        current = match step.side {
            Side::Left => node_hash(&sibling, &current),
            Side::Right => node_hash(&current, &sibling),
        };
    }
    Ok(current == *root)
}
