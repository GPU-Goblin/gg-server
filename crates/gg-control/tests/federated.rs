//! Federated validation (P6, ADR-0014) over HTTP against a real PostgreSQL.
//!
//! What these tests establish:
//! * an audited submission is paid only after two **administratively distinct** validators accepted,
//!   and the receipt names both signed decisions;
//! * several keys of one operator are one opinion: the second key cannot take the audit;
//! * a disagreement opens a dispute — no payment, no sanction, no re-queue — and only an operator
//!   settles it, on an outcome a validator actually signed;
//! * nothing in a worker-facing answer reveals that a submission is audited.

use std::collections::BTreeMap;
use std::path::PathBuf;

use gg_compute::engine::EngineSelection;
use gg_control::config::ServerConfig;
use gg_control::server::{Running, serve};
use gg_control::state::AppState;
use gg_core::*;
use gg_protocol::domain::Domain;
use gg_protocol::keys::{PublicKey, SecretKey};
use gg_protocol::messages::*;
use gg_protocol::signed::Signed;
use gg_protocol::tensor_file::TensorF32;
use gg_storage::queries::identities;
use gg_storage::testing::TestDb;
use serde_json::{Value, json};

struct Env {
    tdb: Option<TestDb>,
    running: Option<Running>,
    http: reqwest::Client,
    base: String,
    admin: String,
    network: NetworkId,
    server_key: PublicKey,
    _tmp: tempfile::TempDir,
}

async fn start(audit_rate: f64) -> Env {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("gg_control=debug,error")
        .with_test_writer()
        .try_init();
    let tdb = TestDb::create_or_panic().await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = ServerConfig {
        database_url: tdb.url(),
        data_dir: tmp.path().to_path_buf(),
        network_id: "gg-e2e".into(),
        lease_secs: 120,
        lease_max_secs: 3600,
        reaper_interval_secs: 5,
        claim_wait_max_secs: 5,
        audit_rate,
        ..ServerConfig::default()
    };
    let state = AppState::new(cfg, tdb.db.clone()).await.unwrap();
    let server_key = state.server_key().public_key();
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
        network: NetworkId::new("gg-e2e").unwrap(),
        server_key,
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
    async fn post(&self, token: &str, path: &str, body: Value) -> (u16, Value) {
        let r = self
            .http
            .post(format!("{}/v1{path}", self.base))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }
    async fn get(&self, token: &str, path: &str) -> (u16, Value) {
        let r = self
            .http
            .get(format!("{}/v1{path}", self.base))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }
    async fn get_bytes(&self, token: &str, path: &str) -> (u16, Vec<u8>) {
        let r = self
            .http
            .get(format!("{}/v1{path}", self.base))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        (status, r.bytes().await.unwrap().to_vec())
    }
    async fn put_bytes(&self, token: &str, path: &str, bytes: Vec<u8>) -> (u16, Value) {
        let r = self
            .http
            .put(format!("{}/v1{path}", self.base))
            .bearer_auth(token)
            .body(bytes)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }
    async fn login(&self, key: &SecretKey, action: &str) -> String {
        let path = if action == "worker-login" {
            "/worker"
        } else {
            "/validator"
        };
        let (s, ch) = self
            .post(
                "",
                &format!("{path}/challenge"),
                json!({ "public_key": key.public_key().to_hex() }),
            )
            .await;
        assert_eq!(s, 200, "{ch}");
        let nonce = hex::decode(ch["nonce"].as_str().unwrap()).unwrap();
        let body = LoginChallengeV1 {
            protocol_version: gg_protocol::PROTOCOL_VERSION,
            network: self.network.clone(),
            action: action.into(),
            nonce,
            public_key: key.public_key(),
            issued_at: UnixMillis::now(),
        };
        let signed = Signed::sign(key, &self.network, Domain::Login, body).unwrap();
        let (s, sess) = self
            .post(
                "",
                &format!("{path}/session"),
                serde_json::to_value(&signed).unwrap(),
            )
            .await;
        assert_eq!(s, 200, "{sess}");
        sess["token"].as_str().unwrap().to_owned()
    }
}

struct Researcher {
    token: String,
    project: ProjectId,
}

async fn setup_researcher(env: &Env, grant: u64) -> Researcher {
    let (s, r) = env
        .post(
            &env.admin,
            "/admin/researchers",
            json!({ "display_name": "Ada", "admission": "approved" }),
        )
        .await;
    assert_eq!(s, 200, "{r}");
    let token = r["token"].as_str().unwrap().to_owned();
    let id: ResearcherId = r["researcher_id"].as_str().unwrap().parse().unwrap();
    let (s, p) = env
        .post(&env.admin, "/admin/projects", json!({ "name": "demo", "allowed_kinds": ["tensor.matmul.v1"], "owner_researcher_id": id, "data_policy_ack": true }))
        .await;
    assert_eq!(s, 200, "{p}");
    let project: ProjectId = p["project_id"].as_str().unwrap().parse().unwrap();
    let (s, b) = env
        .post(
            &env.admin,
            &format!("/admin/projects/{project}/grant"),
            json!({ "amount": grant.to_string(), "reference": "test" }),
        )
        .await;
    assert_eq!(s, 200, "{b}");
    Researcher { token, project }
}

struct WorkerCtx {
    key: SecretKey,
    token: String,
    id: WorkerId,
    dir: tempfile::TempDir,
}

async fn setup_worker(env: &Env, name: &str) -> WorkerCtx {
    let key = SecretKey::generate().unwrap();
    let (s, reg) = env
        .post(
            "",
            "/worker/register",
            json!({ "public_key": key.public_key().to_hex(), "display_name": name }),
        )
        .await;
    assert_eq!(s, 200, "{reg}");
    let id: WorkerId = reg["worker_id"].as_str().unwrap().parse().unwrap();
    let token = env.login(&key, "worker-login").await;
    let (s, _) = env
        .post(
            &env.admin,
            &format!("/admin/workers/{id}/authorize"),
            json!({ "authorized": true }),
        )
        .await;
    assert_eq!(s, 204);
    WorkerCtx {
        key,
        token,
        id,
        dir: tempfile::tempdir().unwrap(),
    }
}

struct ValidatorCtx {
    key: SecretKey,
    token: String,
    id: ValidatorId,
    operator: String,
    dir: tempfile::TempDir,
}

/// Register a validator under a named operator. Two validators sharing an operator name are two
/// keys of one administrative entity.
async fn setup_validator(env: &Env, operator: &str) -> ValidatorCtx {
    let key = SecretKey::generate().unwrap();
    let (s, v) = env
        .post(
            &env.admin,
            "/admin/validators",
            json!({ "public_key": key.public_key().to_hex(), "display_name": operator, "operator": operator }),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let id: ValidatorId = v["validator_id"].as_str().unwrap().parse().unwrap();
    let token = env.login(&key, "validator-login").await;
    ValidatorCtx {
        key,
        token,
        id,
        operator: operator.to_owned(),
        dir: tempfile::tempdir().unwrap(),
    }
}

async fn submit_job(env: &Env, r: &Researcher, seed: u64) -> JobId {
    let mut d = gg_protocol::drbg::Drbg::new("federated", seed);
    let a = TensorF32::new(vec![32, 16], d.sym_f32_vec(32 * 16)).unwrap();
    let b = TensorF32::new(vec![16, 8], d.sym_f32_vec(16 * 8)).unwrap();
    let mut refs = Vec::new();
    for t in [&a, &b] {
        let bytes = t.encode();
        let hash = Sha256Digest::of_bytes(&bytes);
        let size = bytes.len() as u64;
        let (s, resp) = env
            .put_bytes(
                &r.token,
                &format!(
                    "/researcher/projects/{}/artifacts/{hash}?size={size}&schema=ggt1-f32",
                    r.project
                ),
                bytes,
            )
            .await;
        assert_eq!(s, 200, "{resp}");
        refs.push(ArtifactRef { hash, size });
    }
    let def = gg_compute::catalog::matmul_definition(32, 16, 8, refs[0], refs[1]).unwrap();
    let (s, job) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/jobs", r.project),
            json!({ "name": "mm", "tasks": [def] }),
        )
        .await;
    assert_eq!(s, 200, "{job}");
    job["job_id"].as_str().unwrap().parse().unwrap()
}

/// Claim, compute (optionally corrupting the output), upload and submit.
async fn worker_run(env: &Env, w: &WorkerCtx, corrupt: bool) -> Value {
    let (s, claim) = env
        .post(
            &w.token,
            "/worker/claim",
            json!({ "kinds": ["tensor.matmul.v1"], "wait_secs": 2 }),
        )
        .await;
    assert_eq!(s, 200, "{claim}");
    let assignment: Signed<AssignmentV1> =
        serde_json::from_value(claim["assignment"].clone()).unwrap();
    let spec: TaskSpecV1 = serde_json::from_value(claim["spec"].clone()).unwrap();
    let task = gg_compute::validate_definition(&spec.definition).unwrap();
    let mut inputs = BTreeMap::new();
    for inp in claim["inputs"].as_array().unwrap() {
        let name = inp["name"].as_str().unwrap();
        let hash = inp["hash"].as_str().unwrap();
        let (s, bytes) = env
            .get_bytes(&w.token, &format!("/worker/artifacts/{hash}"))
            .await;
        assert_eq!(s, 200);
        let p: PathBuf = w
            .dir
            .path()
            .join(format!("{}-{name}.ggt", assignment.body.attempt_id));
        std::fs::write(&p, bytes).unwrap();
        inputs.insert(name.to_owned(), p);
    }
    let out_dir = w
        .dir
        .path()
        .join(format!("out-{}", assignment.body.attempt_id));
    let mut report =
        gg_compute::matmul::execute(EngineSelection::FlexCpu, &task, &inputs, &out_dir).unwrap();
    if corrupt {
        let c = report.outputs.get_mut("c").unwrap();
        let mut t = TensorF32::read_from_path(&c.path, &Default::default()).unwrap();
        t.data[0] += 1.0;
        let bytes = t.encode();
        std::fs::write(&c.path, &bytes).unwrap();
        c.hash = Sha256Digest::of_bytes(&bytes);
        c.size = bytes.len() as u64;
    }
    for f in report.outputs.values() {
        let bytes = std::fs::read(&f.path).unwrap();
        let (s, a) = env
            .put_bytes(
                &w.token,
                &format!(
                    "/worker/attempts/{}/outputs/{}?size={}&schema=ggt1-f32",
                    assignment.body.attempt_id, f.hash, f.size
                ),
                bytes,
            )
            .await;
        assert_eq!(s, 200, "{a}");
    }
    let body = ResultSubmissionV1 {
        protocol_version: gg_protocol::PROTOCOL_VERSION,
        network: env.network.clone(),
        assignment_hash: assignment
            .body_hash(&env.network, Domain::Assignment)
            .unwrap(),
        task_id: assignment.body.task_id,
        attempt_id: assignment.body.attempt_id,
        spec_hash: assignment.body.spec_hash,
        generation: assignment.body.generation,
        outputs: report
            .outputs
            .iter()
            .map(|(n, f)| (n.clone(), f.artifact_ref()))
            .collect(),
        execution_profile: report.engine.to_profile(report.duration_ms),
        declared_metrics: report.metrics.clone(),
        submitted_at: UnixMillis::now(),
    };
    let signed = Signed::sign(&w.key, &env.network, Domain::Result, body).unwrap();
    let (s, resp) = env
        .post(
            &w.token,
            &format!("/worker/attempts/{}/result", signed.body.attempt_id),
            serde_json::to_value(&signed).unwrap(),
        )
        .await;
    assert_eq!(s, 200, "{resp}");
    resp
}

/// Claim one verification and decide. `honest = false` accepts whatever it was given without
/// recomputing: a validator that vouches for anything.
async fn validator_decide(env: &Env, v: &ValidatorCtx, honest: bool) -> Option<(String, Value)> {
    let (s, work) = env
        .post(&v.token, "/validator/claim", json!({ "wait_secs": 0 }))
        .await;
    if s == 204 {
        return None;
    }
    assert_eq!(s, 200, "{work}");
    let spec: TaskSpecV1 = serde_json::from_value(work["spec"].clone()).unwrap();
    let submission: Signed<ResultSubmissionV1> =
        serde_json::from_value(work["submission"].clone()).unwrap();
    let vid = work["verification_id"].as_str().unwrap().to_owned();
    let (checks, artifacts_checked, outcome) = if honest {
        let task = gg_compute::validate_definition(&spec.definition).unwrap();
        let mut inputs = BTreeMap::new();
        let mut outputs = BTreeMap::new();
        for (list, map) in [("inputs", &mut inputs), ("outputs", &mut outputs)] {
            for item in work[list].as_array().unwrap() {
                let name = item["name"].as_str().unwrap();
                let hash = item["hash"].as_str().unwrap();
                let (s, bytes) = env
                    .get_bytes(&v.token, &format!("/validator/artifacts/{hash}"))
                    .await;
                assert_eq!(s, 200);
                let p = v.dir.path().join(format!("{vid}-{list}-{name}"));
                std::fs::write(&p, bytes).unwrap();
                map.insert(name.to_owned(), p);
            }
        }
        let verdict = gg_compute::verify_task(
            &task,
            &inputs,
            &outputs,
            &submission.body.outputs,
            &submission.body.execution_profile.numeric_profile,
        )
        .unwrap();
        (verdict.checks, verdict.artifacts_checked, verdict.outcome)
    } else {
        (
            vec![CheckV1 {
                name: "vouched".into(),
                passed: true,
                measured: None,
                tolerance: None,
                detail: "accepted without recomputing".into(),
            }],
            submission.body.outputs.values().copied().collect(),
            VerificationOutcome::Accepted,
        )
    };
    let decision = VerificationDecisionV1 {
        protocol_version: gg_protocol::PROTOCOL_VERSION,
        network: env.network.clone(),
        decision_id: DecisionId::new(),
        submission_hash: submission.body_hash(&env.network, Domain::Result).unwrap(),
        task_id: spec.task_id,
        attempt_id: submission.body.attempt_id,
        validator_id: v.id,
        method: if honest {
            "recompute-reference".into()
        } else {
            "declared".into()
        },
        method_version: 1,
        reference_profile: gg_compute::profiles::REFERENCE_PROFILE.into(),
        checks,
        artifacts_checked,
        outcome,
        decided_at: UnixMillis::now(),
    };
    let signed = Signed::sign(&v.key, &env.network, Domain::Verification, decision).unwrap();
    let (s, resp) = env
        .post(
            &v.token,
            &format!("/validator/verifications/{vid}/decision"),
            serde_json::to_value(&signed).unwrap(),
        )
        .await;
    assert_eq!(s, 200, "{resp}");
    Some((vid, resp))
}

#[tokio::test]
async fn an_audited_submission_is_paid_only_after_two_operators_accepted() {
    let env = start(1.0).await; // every submission is audited
    let r = setup_researcher(&env, 100).await;
    let w = setup_worker(&env, "w1").await;
    let a = setup_validator(&env, "op-a").await;
    let b = setup_validator(&env, "op-b").await;
    let _job = submit_job(&env, &r, 1).await;

    let submitted = worker_run(&env, &w, false).await;
    // The worker's answer says nothing about audits: the schedule is not communicated.
    let text = submitted.to_string();
    for leak in ["audit", "awaiting", "operator"] {
        assert!(
            !text.contains(leak),
            "submission answer leaks '{leak}': {text}"
        );
    }

    // First acceptance does not pay: the audit is still open.
    let (_vid, first) = validator_decide(&env, &a, true).await.expect("primary");
    assert_eq!(first["outcome"], "accepted");
    assert_eq!(first["applied"], false, "{first}");
    assert_eq!(first["awaiting"], 1);
    assert!(first["receipt_id"].is_null());
    let (_s, proj) = env
        .get(&r.token, &format!("/researcher/projects/{}", r.project))
        .await;
    assert_eq!(proj["budget"]["spent"], "0", "nothing is paid yet");

    // Second acceptance, by another operator, finalizes and pays exactly once.
    let (_vid, second) = validator_decide(&env, &b, true).await.expect("audit");
    assert_eq!(second["outcome"], "accepted");
    assert_eq!(second["applied"], true, "{second}");
    assert_eq!(second["awaiting"], 0);
    assert!(second["receipt_id"].is_string(), "{second}");
    assert_eq!(second["task_state"], "accepted");

    // The receipt names both signed decisions.
    let (s, receipts) = env
        .get(
            &r.token,
            &format!("/researcher/projects/{}/receipts", r.project),
        )
        .await;
    assert_eq!(s, 200, "{receipts}");
    let list = receipts.as_array().unwrap();
    assert_eq!(list.len(), 1, "{receipts}");
    // Signed by the server and naming both decisions: the payment is verifiable offline as backed
    // by two administratively distinct validators.
    let receipt: Signed<ContributionReceiptV1> =
        serde_json::from_value(list[0]["receipt"].clone()).unwrap();
    receipt.verify(&env.network, Domain::Receipt).unwrap();
    assert_eq!(receipt.signer, env.server_key);
    assert_eq!(receipt.body.worker_id, w.id);
    assert_eq!(receipt.body.decision_hashes.len(), 2, "{receipts}");
    assert!(
        receipt
            .body
            .decision_hashes
            .contains(&receipt.body.decision_hash)
    );
    env.stop().await;
}

#[tokio::test]
async fn two_keys_of_one_operator_are_one_opinion() {
    let env = start(1.0).await;
    let r = setup_researcher(&env, 100).await;
    let w = setup_worker(&env, "w1").await;
    let a1 = setup_validator(&env, "op-a").await;
    let a2 = setup_validator(&env, "op-a").await; // same operator, another key
    let b = setup_validator(&env, "op-b").await;
    assert_eq!(a1.operator, a2.operator);
    let _job = submit_job(&env, &r, 2).await;
    worker_run(&env, &w, false).await;

    validator_decide(&env, &a1, true).await.expect("primary");
    // The second key of the same operator finds nothing to do: it may not audit its own opinion.
    assert!(
        validator_decide(&env, &a2, true).await.is_none(),
        "a second key of the same operator must not take the audit"
    );
    // A validator of another operator can.
    let (_vid, resp) = validator_decide(&env, &b, true).await.expect("audit");
    assert_eq!(resp["applied"], true, "{resp}");
    assert_eq!(resp["task_state"], "accepted");
    env.stop().await;
}

#[tokio::test]
async fn a_disagreement_opens_a_dispute_that_only_an_operator_settles() {
    let env = start(1.0).await;
    let r = setup_researcher(&env, 100).await;
    let w = setup_worker(&env, "dishonest").await;
    let honest = setup_validator(&env, "op-a").await;
    let vouching = setup_validator(&env, "op-b").await;
    let job = submit_job(&env, &r, 3).await;
    worker_run(&env, &w, true).await; // corrupted output

    // One validator recomputes and rejects; the other vouches for the result without recomputing.
    let (_v1, first) = validator_decide(&env, &honest, true)
        .await
        .expect("primary");
    assert_eq!(first["outcome"], "rejected");
    assert_eq!(first["applied"], false, "nothing is decided alone: {first}");
    let (_v2, second) = validator_decide(&env, &vouching, false)
        .await
        .expect("audit");
    assert_eq!(second["outcome"], "accepted");
    assert_eq!(second["task_state"], "disputed", "{second}");
    assert!(second["dispute_id"].is_string(), "{second}");
    assert!(second["receipt_id"].is_null(), "a divergence pays nothing");

    // No payment, no sanction, and the task is not re-queued behind the operator's back.
    let (_s, proj) = env
        .get(&r.token, &format!("/researcher/projects/{}", r.project))
        .await;
    assert_eq!(proj["budget"]["spent"], "0");
    assert_ne!(proj["budget"]["reserved"], "0", "the reservation is held");

    // The dispute is visible to operators with both signed opinions.
    let (s, disputes) = env.get(&env.admin, "/admin/disputes").await;
    assert_eq!(s, 200, "{disputes}");
    let list = disputes.as_array().unwrap();
    assert_eq!(list.len(), 1, "{disputes}");
    let dispute_id = list[0]["dispute_id"].as_str().unwrap().to_owned();
    let opinions = list[0]["opinions"].as_array().unwrap();
    assert_eq!(opinions.len(), 2);
    let operators: Vec<&str> = opinions
        .iter()
        .filter_map(|o| o["operator_id"].as_str())
        .collect();
    assert_eq!(
        operators.len(),
        2,
        "both opinions carry their administrative operator"
    );
    assert_ne!(operators[0], operators[1]);

    // A researcher sees the audit trail of the disputed submission.
    let (_s, job_view) = env.get(&r.token, &format!("/researcher/jobs/{job}")).await;
    let task_id = job_view["tasks"][0]["task_id"].as_str().unwrap().to_owned();
    let (_s, detail) = env
        .get(&r.token, &format!("/researcher/tasks/{task_id}"))
        .await;
    assert_eq!(detail["task"]["state"], "disputed");
    assert_eq!(detail["verifications"].as_array().unwrap().len(), 2);

    // An operator may not settle on an outcome nobody signed.
    let (s, e) = env
        .post(
            &env.admin,
            &format!("/admin/disputes/{dispute_id}/resolve"),
            json!({ "outcome": "inconclusive", "reason": "no" }),
        )
        .await;
    assert_eq!(s, 422, "{e}");

    // Settling on the rejection: still no payment, and the task returns to the queue.
    let (s, resolved) = env
        .post(
            &env.admin,
            &format!("/admin/disputes/{dispute_id}/resolve"),
            json!({ "outcome": "rejected", "reason": "recomputation shows a wrong product" }),
        )
        .await;
    assert_eq!(s, 200, "{resolved}");
    assert_eq!(resolved["outcome"], "rejected");
    assert_eq!(resolved["task_state"], "queued");
    assert!(resolved["receipt_id"].is_null());
    let (_s, proj) = env
        .get(&r.token, &format!("/researcher/projects/{}", r.project))
        .await;
    assert_eq!(proj["budget"]["spent"], "0");
    assert_eq!(
        proj["budget"]["reserved"], "0",
        "the reservation is released"
    );

    // Settling twice is refused, and the decision is in the audit log.
    let (s, again) = env
        .post(
            &env.admin,
            &format!("/admin/disputes/{dispute_id}/resolve"),
            json!({ "outcome": "rejected", "reason": "again" }),
        )
        .await;
    assert_eq!(s, 409, "{again}");
    let (_s, audit) = env.get(&env.admin, "/admin/audit").await;
    let entries = audit.as_array().unwrap();
    assert!(
        entries.iter().any(|e| e["action"] == "dispute.resolve"),
        "the settlement is audited: {audit}"
    );

    // An honest worker can now take the task and be paid normally.
    let good = setup_worker(&env, "honest").await;
    worker_run(&env, &good, false).await;
    validator_decide(&env, &honest, true)
        .await
        .expect("primary");
    let (_v, final_resp) = validator_decide(&env, &vouching, false)
        .await
        .expect("audit");
    assert_eq!(final_resp["task_state"], "accepted", "{final_resp}");
    env.stop().await;
}

#[tokio::test]
async fn a_single_operator_deployment_keeps_working_without_audits() {
    // audit_rate = 1.0 but only one operator can validate: opening an audit nobody may claim would
    // stall every task, so none is opened.
    let env = start(1.0).await;
    let r = setup_researcher(&env, 100).await;
    let w = setup_worker(&env, "w1").await;
    let only = setup_validator(&env, "op-a").await;
    let _job = submit_job(&env, &r, 4).await;
    worker_run(&env, &w, false).await;
    let (_vid, resp) = validator_decide(&env, &only, true).await.expect("primary");
    assert_eq!(resp["applied"], true, "{resp}");
    assert_eq!(resp["awaiting"], 0);
    assert_eq!(resp["task_state"], "accepted");
    assert!(resp["receipt_id"].is_string());
    env.stop().await;
}

#[tokio::test]
async fn audits_are_not_opened_when_the_rate_is_zero() {
    let env = start(0.0).await;
    let r = setup_researcher(&env, 100).await;
    let w = setup_worker(&env, "w1").await;
    let a = setup_validator(&env, "op-a").await;
    let b = setup_validator(&env, "op-b").await;
    let _job = submit_job(&env, &r, 5).await;
    worker_run(&env, &w, false).await;
    let (_vid, resp) = validator_decide(&env, &a, true).await.expect("primary");
    assert_eq!(resp["applied"], true, "{resp}");
    assert_eq!(resp["task_state"], "accepted");
    // Nothing is left for the second operator.
    assert!(validator_decide(&env, &b, true).await.is_none());
    env.stop().await;
}

#[tokio::test]
async fn the_audit_rate_is_changed_while_the_server_runs() {
    // Start with no audit at all, then raise the rate without redeploying (docs/DECISIONS.md, D2).
    let env = start(0.0).await;
    let r = setup_researcher(&env, 200).await;
    let w = setup_worker(&env, "w1").await;
    let a = setup_validator(&env, "op-a").await;
    let b = setup_validator(&env, "op-b").await;

    let (s, rate) = env.get(&env.admin, "/admin/config/audit-rate").await;
    assert_eq!(s, 200, "{rate}");
    assert_eq!(rate["audit_rate"], 0.0);

    // At rate 0, one acceptance settles the task.
    let _job = submit_job(&env, &r, 11).await;
    worker_run(&env, &w, false).await;
    let (_vid, resp) = validator_decide(&env, &a, true).await.expect("primary");
    assert_eq!(resp["task_state"], "accepted", "{resp}");

    // An out-of-range value is refused rather than clamped silently.
    for bad in [-0.1, 1.5] {
        let (s, e) = env
            .post(
                &env.admin,
                "/admin/config/audit-rate",
                json!({ "rate": bad, "reason": "test" }),
            )
            .await;
        assert_eq!(s, 422, "rate {bad} was accepted: {e}");
    }

    // Raise it to 1.0: the next submission needs two distinct operators.
    let (s, applied) = env
        .post(
            &env.admin,
            "/admin/config/audit-rate",
            json!({ "rate": 1.0, "reason": "suspicion about a contributor" }),
        )
        .await;
    assert_eq!(s, 200, "{applied}");
    assert_eq!(applied["audit_rate"], 1.0);
    assert_eq!(
        applied["configured_at_start"], 0.0,
        "the start-up value is still reported"
    );

    let _job = submit_job(&env, &r, 12).await;
    worker_run(&env, &w, false).await;
    let (_vid, first) = validator_decide(&env, &a, true).await.expect("primary");
    assert_eq!(first["applied"], false, "an audit is now pending: {first}");
    assert_eq!(first["awaiting"], 1);
    let (_vid, second) = validator_decide(&env, &b, true).await.expect("audit");
    assert_eq!(second["task_state"], "accepted", "{second}");

    // The change is in the audit log with its reason.
    let (_s, audit) = env.get(&env.admin, "/admin/audit").await;
    let entry = audit
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["action"] == "config.audit_rate")
        .expect("the change is audited");
    assert!(
        entry["detail"]["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("suspicion"),
        "{entry}"
    );
    env.stop().await;
}

/// A validator suspected of being compromised is withdrawn (INCIDENTS.md): its session stops
/// working at once, the verification it was holding goes back to the queue, and another validator
/// decides it. Nothing waits for its lease to run out.
#[tokio::test]
async fn a_withdrawn_validator_loses_its_session_and_its_verification_goes_to_another() {
    let env = start(0.0).await;
    let r = setup_researcher(&env, 100).await;
    let w = setup_worker(&env, "w1").await;
    let suspect = setup_validator(&env, "op-a").await;
    let other = setup_validator(&env, "op-b").await;
    let _job = submit_job(&env, &r, 11).await;
    worker_run(&env, &w, false).await;

    // The suspect claims the verification and holds it.
    let (s, work) = env
        .post(
            &suspect.token,
            "/validator/claim",
            json!({ "wait_secs": 0 }),
        )
        .await;
    assert_eq!(s, 200, "{work}");
    let held = work["verification_id"].as_str().unwrap().to_owned();
    // Nothing else to claim while it holds it.
    let (s, _) = env
        .post(&other.token, "/validator/claim", json!({ "wait_secs": 0 }))
        .await;
    assert_eq!(s, 204);

    let (s, withdrawn) = env
        .post(
            &env.admin,
            &format!("/admin/validators/{}/authorize", suspect.id),
            json!({ "authorized": false }),
        )
        .await;
    assert_eq!(s, 200, "{withdrawn}");
    assert_eq!(withdrawn["released_verifications"], 1, "{withdrawn}");

    // Its session no longer works: no claim, no decision.
    let (s, _) = env
        .post(
            &suspect.token,
            "/validator/claim",
            json!({ "wait_secs": 0 }),
        )
        .await;
    assert!(s == 401 || s == 403, "withdrawn validator claimed: {s}");
    let (s, _) = env
        .post(
            &suspect.token,
            &format!("/validator/verifications/{held}/decision"),
            json!({}),
        )
        .await;
    assert!(s == 401 || s == 403, "withdrawn validator decided: {s}");
    // Logging in again yields a session that can do nothing, like an unauthorized worker's.
    let fresh = env.login(&suspect.key, "validator-login").await;
    let (s, _) = env
        .post(&fresh, "/validator/claim", json!({ "wait_secs": 0 }))
        .await;
    assert_eq!(
        s, 403,
        "a fresh session of a withdrawn validator must not claim"
    );

    // The other validator gets the same verification and settles the task.
    let (vid, resp) = validator_decide(&env, &other, true)
        .await
        .expect("the released verification is claimable");
    assert_eq!(vid, held);
    assert_eq!(resp["task_state"], "accepted", "{resp}");
    assert!(resp["receipt_id"].is_string());
    env.stop().await;
}
