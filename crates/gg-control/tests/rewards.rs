//! Deferred settlement (P7, ADR-0015) over HTTP against a real PostgreSQL.
//!
//! What these tests establish: a batch commits the accepted contributions exactly once, its proofs
//! verify with the crate the contract uses, an obligation cannot be committed twice, and the
//! settlement machine refuses to consider a batch settled on a transaction hash alone.

use gg_control::config::ServerConfig;
use gg_control::server::{Running, serve};
use gg_control::state::AppState;
use gg_core::*;
use gg_merkle::{ProofStep, RewardLeafV1, verify_proof};
use gg_storage::queries::identities;
use gg_storage::testing::TestDb;
use serde_json::{Value, json};

struct Env {
    tdb: Option<TestDb>,
    running: Option<Running>,
    http: reqwest::Client,
    base: String,
    admin: String,
    _tmp: tempfile::TempDir,
}

async fn start() -> Env {
    let tdb = TestDb::create_or_panic().await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = ServerConfig {
        database_url: tdb.url(),
        data_dir: tmp.path().to_path_buf(),
        network_id: "gg-e2e".into(),
        ..ServerConfig::default()
    };
    let state = AppState::new(cfg, tdb.db.clone()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let running = serve(state, listener).await.unwrap();
    let admin = gg_control::auth::new_token(gg_control::auth::API_TOKEN_PREFIX).unwrap();
    let mut c = tdb.db.pool().acquire().await.unwrap();
    identities::create_token(
        &mut c,
        &gg_control::auth::token_hash(&admin),
        "admin",
        None,
        "test",
        None,
    )
    .await
    .unwrap();
    drop(c);
    Env {
        base: running.url(),
        running: Some(running),
        tdb: Some(tdb),
        http: reqwest::Client::new(),
        admin,
        _tmp: tmp,
    }
}

impl Env {
    async fn stop(mut self) {
        if let Some(r) = self.running.take() {
            r.stop().await;
        }
        if let Some(t) = self.tdb.take() {
            t.drop().await;
        }
    }
    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let r = self
            .http
            .post(format!("{}/v1{path}", self.base))
            .bearer_auth(&self.admin)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }
    async fn get(&self, path: &str) -> (u16, Value) {
        let r = self
            .http
            .get(format!("{}/v1{path}", self.base))
            .bearer_auth(&self.admin)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }
    /// Insert accepted contributions directly: this test is about settlement, not about the path
    /// that produced the receipts (covered by the end-to-end tests).
    async fn seed_receipts(&self, count: usize, with_payout: bool) -> Vec<ReceiptId> {
        use gg_protocol::domain::Domain;
        use gg_protocol::keys::SecretKey;
        use gg_protocol::messages::*;
        use gg_protocol::signed::Signed;
        use gg_storage::queries::{jobs, tasks};

        let db = &self.tdb.as_ref().unwrap().db;
        let mut c = db.pool().acquire().await.unwrap();
        let network = NetworkId::new("gg-e2e").unwrap();
        let project = ProjectId::new();
        identities::create_project(&mut c, project, "p", ProjectState::Active, &[])
            .await
            .unwrap();
        let experiment = ExperimentId::new();
        jobs::create_experiment(&mut c, experiment, project, "e", None)
            .await
            .unwrap();
        let job = JobId::new();
        jobs::create_job(&mut c, job, project, experiment, None, "j")
            .await
            .unwrap();
        let mut out = Vec::new();
        for i in 0..count {
            let key = SecretKey::generate().unwrap();
            let worker = WorkerId::new();
            identities::create_worker(&mut c, worker, &key.public_key().0, &format!("w{i}"))
                .await
                .unwrap();
            if with_payout {
                identities::set_payout_address(
                    &mut c,
                    worker,
                    &format!("cosmwasm1beneficiary{i:04}"),
                )
                .await
                .unwrap();
            }
            let task = TaskId::new();
            let def = gg_compute::catalog::matmul_definition(
                2,
                2,
                2,
                ArtifactRef {
                    hash: Sha256Digest([1; 32]),
                    size: 6 + 16 + 4 * 4,
                },
                ArtifactRef {
                    hash: Sha256Digest([2; 32]),
                    size: 6 + 16 + 4 * 4,
                },
            )
            .unwrap();
            let spec = TaskSpecV1 {
                spec_version: 1,
                network: network.clone(),
                project_id: project,
                experiment_id: experiment,
                job_id: job,
                task_id: task,
                index: i as u32,
                definition: def.clone(),
            };
            let spec_cbor = gg_protocol::canonical::to_bytes(&spec).unwrap();
            jobs::insert_task(
                &mut c,
                &jobs::NewTask {
                    id: task,
                    job_id: job,
                    project_id: project,
                    experiment_id: experiment,
                    index: i as i32,
                    compute_kind: "tensor.matmul.v1",
                    spec_hash: Sha256Digest([3; 32]),
                    definition_hash: Sha256Digest([4; 32]),
                    spec_cbor: &spec_cbor,
                    max_attempts: 3,
                    priority: 0,
                    reward_primary: Credits(2),
                    verification_cost: Credits(1),
                    est_cpu_secs: 0.1,
                    requirements: Default::default(),
                },
            )
            .await
            .unwrap();
            let attempt = AttemptId::new();
            tasks::insert_attempt(
                &mut c,
                attempt,
                task,
                worker,
                1,
                AttemptRole::Primary,
                &[],
                &Sha256Digest([7; 32]),
            )
            .await
            .unwrap();
            let rid = ReceiptId::new();
            let receipt = ContributionReceiptV1 {
                protocol_version: gg_protocol::PROTOCOL_VERSION,
                network: network.clone(),
                receipt_id: rid,
                project_id: project,
                experiment_id: experiment,
                job_id: job,
                task_id: task,
                attempt_id: attempt,
                role: AttemptRole::Primary,
                spec_hash: Sha256Digest([3; 32]),
                definition_hash: Sha256Digest([4; 32]),
                input_hashes: vec![],
                submission_hash: Sha256Digest([5; 32]),
                output_hashes: vec![],
                decision_hash: Sha256Digest([6; 32]),
                decision_hashes: vec![Sha256Digest([6; 32])],
                worker_id: worker,
                worker_public_key: key.public_key(),
                beneficiary: None,
                amount: Credits(2 + i as u64),
                asset: "TC".into(),
                issued_at: UnixMillis::now(),
            };
            let signed = Signed::sign(&key, &network, Domain::Receipt, receipt).unwrap();
            let hash = signed.body_hash(&network, Domain::Receipt).unwrap();
            tasks::insert_receipt(
                &mut c,
                rid,
                attempt,
                worker,
                project,
                AttemptRole::Primary,
                Credits(2 + i as u64),
                None,
                &hash,
                &signed.to_canonical_bytes().unwrap(),
            )
            .await
            .unwrap();
            out.push(rid);
        }
        out
    }
}

fn leaf_of(entry: &Value) -> RewardLeafV1 {
    serde_json::from_value(entry["leaf"].clone()).expect("leaf")
}
fn proof_of(entry: &Value) -> Vec<ProofStep> {
    serde_json::from_value(entry["proof"].clone()).expect("proof")
}

#[tokio::test]
async fn a_batch_commits_each_accepted_contribution_once_with_a_verifiable_proof() {
    let env = start().await;
    let receipts = env.seed_receipts(5, true).await;

    // A dry run shows the commitment without writing it.
    let (s, dry) = env
        .post(
            "/admin/reward-batches",
            json!({ "batch_id": "batch-1", "chain_id": "gg-localnet-1", "contract": "cosmwasm1contract", "asset": "utrg", "dry_run": true }),
        )
        .await;
    assert_eq!(s, 200, "{dry}");
    assert_eq!(dry["leaf_count"], 5);
    let (s, listed) = env.get("/admin/reward-batches").await;
    assert_eq!(s, 200, "{listed}");
    assert_eq!(
        listed.as_array().unwrap().len(),
        0,
        "a dry run writes nothing"
    );

    let (s, batch) = env
        .post(
            "/admin/reward-batches",
            json!({ "batch_id": "batch-1", "chain_id": "gg-localnet-1", "contract": "cosmwasm1contract", "asset": "utrg" }),
        )
        .await;
    assert_eq!(s, 200, "{batch}");
    assert_eq!(batch["state"], "prepared");
    assert_eq!(batch["leaf_count"], 5);
    // Amounts 2..=6.
    assert_eq!(batch["total_amount"], "20");
    assert_eq!(batch["merkle_root"].as_str().unwrap().len(), 64);

    // Every proof verifies with the crate the contract runs.
    let root: [u8; 32] = hex::decode(batch["merkle_root"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let leaves = batch["leaves"].as_array().unwrap();
    assert_eq!(leaves.len(), 5);
    for entry in leaves {
        let leaf = leaf_of(entry);
        assert_eq!(leaf.batch_id, "batch-1");
        assert_eq!(leaf.chain_id, "gg-localnet-1");
        assert_eq!(leaf.network, "gg-e2e");
        assert!(leaf.beneficiary.starts_with("cosmwasm1beneficiary"));
        assert!(
            verify_proof(&leaf, &proof_of(entry), &root).unwrap(),
            "proof of {} must verify",
            leaf.reward_id
        );
    }
    // Each committed obligation is a receipt, exactly once.
    let committed: Vec<String> = leaves
        .iter()
        .map(|e| leaf_of(e).reward_id)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(committed.len(), 5);
    for r in &receipts {
        assert!(committed.contains(&r.to_string()));
    }

    // Nothing is left to commit, and the same identifier cannot be reused.
    let (s, e) = env
        .post(
            "/admin/reward-batches",
            json!({ "batch_id": "batch-2", "chain_id": "gg-localnet-1", "contract": "cosmwasm1contract", "asset": "utrg" }),
        )
        .await;
    assert_eq!(s, 422, "{e}");
    let (s, e) = env
        .post(
            "/admin/reward-batches",
            json!({ "batch_id": "batch-1", "chain_id": "gg-localnet-1", "contract": "cosmwasm1contract", "asset": "utrg" }),
        )
        .await;
    assert_eq!(s, 409, "{e}");

    // The export is stable: same root, same proofs, read back from storage.
    let (s, again) = env.get("/admin/reward-batches/batch-1").await;
    assert_eq!(s, 200, "{again}");
    assert_eq!(again["merkle_root"], batch["merkle_root"]);
    for entry in again["leaves"].as_array().unwrap() {
        assert!(verify_proof(&leaf_of(entry), &proof_of(entry), &root).unwrap());
    }
    env.stop().await;
}

#[tokio::test]
async fn a_contribution_without_a_declared_payout_address_is_never_committed() {
    let env = start().await;
    env.seed_receipts(3, false).await;
    let (s, e) = env
        .post(
            "/admin/reward-batches",
            json!({ "batch_id": "batch-1", "chain_id": "gg-localnet-1", "contract": "cosmwasm1contract", "asset": "utrg" }),
        )
        .await;
    assert_eq!(s, 422, "{e}");
    assert!(
        e["message"].as_str().unwrap_or_default().contains("payout"),
        "{e}"
    );
    env.stop().await;
}

#[tokio::test]
async fn a_transaction_hash_alone_never_settles_a_batch() {
    let env = start().await;
    env.seed_receipts(2, true).await;
    let (s, batch) = env
        .post(
            "/admin/reward-batches",
            json!({ "batch_id": "batch-1", "chain_id": "gg-localnet-1", "contract": "cosmwasm1contract", "asset": "utrg" }),
        )
        .await;
    assert_eq!(s, 200, "{batch}");
    let root = batch["merkle_root"].as_str().unwrap().to_owned();

    // Finality cannot be claimed before anything was submitted or observed.
    for event in ["observed", "finalized"] {
        let (s, e) = env
            .post(
                "/admin/reward-batches/batch-1/settlement",
                json!({ "event": event, "observed_root": root }),
            )
            .await;
        assert_eq!(s, 409, "{event}: {e}");
    }
    // A submission reports its transaction hash.
    let (s, e) = env
        .post(
            "/admin/reward-batches/batch-1/settlement",
            json!({ "event": "submitted" }),
        )
        .await;
    assert_eq!(s, 422, "{e}");
    let (s, v) = env
        .post(
            "/admin/reward-batches/batch-1/settlement",
            json!({ "event": "submitted", "tx_hash": "ABC123" }),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["state"], "submitted");
    // A duplicated RPC event changes nothing.
    let (s, v) = env
        .post(
            "/admin/reward-batches/batch-1/settlement",
            json!({ "event": "submitted", "tx_hash": "ABC123" }),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["state"], "submitted");
    // A submission is not finality.
    let (s, e) = env
        .post(
            "/admin/reward-batches/batch-1/settlement",
            json!({ "event": "finalized" }),
        )
        .await;
    assert_eq!(s, 409, "{e}");

    // An observation that does not show the committed root sends the batch to reconciliation.
    let (s, v) = env
        .post(
            "/admin/reward-batches/batch-1/settlement",
            json!({ "event": "observed", "height": 42, "observed_root": "ff".repeat(32) }),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["state"], "to_reconcile");
    assert!(v["note"].as_str().unwrap().contains("differs"), "{v}");

    // After a resubmission, the right observation moves it forward, and only then finality.
    let (s, v) = env
        .post(
            "/admin/reward-batches/batch-1/settlement",
            json!({ "event": "submitted", "tx_hash": "DEF456" }),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = env
        .post(
            "/admin/reward-batches/batch-1/settlement",
            json!({ "event": "observed", "height": 51, "observed_root": root }),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["state"], "observed");
    let (s, v) = env
        .post(
            "/admin/reward-batches/batch-1/settlement",
            json!({ "event": "finalized", "note": "20 blocks" }),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["state"], "finalized");
    // A finalized batch is never reopened.
    let (s, e) = env
        .post(
            "/admin/reward-batches/batch-1/settlement",
            json!({ "event": "reconcile", "note": "too late" }),
        )
        .await;
    assert_eq!(s, 409, "{e}");
    env.stop().await;
}
