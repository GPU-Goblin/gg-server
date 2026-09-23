//! End-to-end tests over HTTP against a real PostgreSQL and object store:
//! submission → assignment → execution → verification → receipt → single ledger write,
//! plus adversarial cases (replay, corrupted result, expired lease, cancellation, cross-project access).

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

async fn start(lease_secs: i64, reaper_secs: u64) -> Env {
    start_with(lease_secs, reaper_secs, |_| {}).await
}

async fn start_with(
    lease_secs: i64,
    reaper_secs: u64,
    tune: impl FnOnce(&mut ServerConfig),
) -> Env {
    let tdb = TestDb::create_or_panic().await;
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = ServerConfig {
        database_url: tdb.url(),
        data_dir: tmp.path().to_path_buf(),
        network_id: "gg-e2e".into(),
        lease_secs,
        lease_max_secs: lease_secs.max(3600),
        reaper_interval_secs: reaper_secs,
        claim_wait_max_secs: 5,
        ..ServerConfig::default()
    };
    tune(&mut cfg);
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

    async fn post_raw(
        &self,
        token: &str,
        path: &str,
        body: Value,
        headers: &[(&str, &str)],
    ) -> (u16, Value) {
        let mut req = self
            .http
            .post(format!("{}/v1{path}", self.base))
            .bearer_auth(token)
            .json(&body);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let r = req.send().await.unwrap();
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
    id: ResearcherId,
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
    assert_eq!(b["available"], grant.to_string());
    Researcher { token, id, project }
}

async fn upload_input(env: &Env, r: &Researcher, t: &TensorF32) -> ArtifactRef {
    let bytes = t.encode();
    let hash = Sha256Digest::of_bytes(&bytes);
    let size = bytes.len() as u64;
    let (s, a) = env
        .put_bytes(
            &r.token,
            &format!(
                "/researcher/projects/{}/artifacts/{hash}?size={size}&schema=ggt1-f32",
                r.project
            ),
            bytes,
        )
        .await;
    assert_eq!(s, 200, "{a}");
    ArtifactRef { hash, size }
}

fn matmul_inputs(seed: u64, m: u32, k: u32, n: u32) -> (TensorF32, TensorF32) {
    let mut d = gg_protocol::drbg::Drbg::new("e2e", seed);
    let a = TensorF32::new(vec![m as u64, k as u64], d.sym_f32_vec((m * k) as usize)).unwrap();
    let b = TensorF32::new(vec![k as u64, n as u64], d.sym_f32_vec((k * n) as usize)).unwrap();
    (a, b)
}

async fn submit_matmul(
    env: &Env,
    r: &Researcher,
    seed: u64,
    dims: (u32, u32, u32),
    headers: &[(&str, &str)],
) -> (u16, Value) {
    let (a, b) = matmul_inputs(seed, dims.0, dims.1, dims.2);
    let ra = upload_input(env, r, &a).await;
    let rb = upload_input(env, r, &b).await;
    let def = gg_compute::catalog::matmul_definition(dims.0, dims.1, dims.2, ra, rb).unwrap();
    env.post_raw(
        &r.token,
        &format!("/researcher/projects/{}/jobs", r.project),
        json!({ "name": "mm", "tasks": [def] }),
        headers,
    )
    .await
}

struct WorkerCtx {
    key: SecretKey,
    token: String,
    id: WorkerId,
    dir: tempfile::TempDir,
}

async fn setup_worker(env: &Env) -> WorkerCtx {
    let key = SecretKey::generate().unwrap();
    let (s, reg) = env
        .post(
            "",
            "/worker/register",
            json!({ "public_key": key.public_key().to_hex(), "display_name": "w1" }),
        )
        .await;
    assert_eq!(s, 200, "{reg}");
    assert_eq!(reg["authorized"], false);
    let id: WorkerId = reg["worker_id"].as_str().unwrap().parse().unwrap();
    // Unauthorized worker can log in but cannot claim.
    let token = env.login(&key, "worker-login").await;
    let (s, _) = env
        .post(
            &token,
            "/worker/claim",
            json!({ "kinds": ["tensor.matmul.v1"], "wait_secs": 0 }),
        )
        .await;
    assert_eq!(s, 403);
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

/// Claim, download inputs, execute on CPU, upload output. Returns (claim json, report).
async fn worker_execute(
    env: &Env,
    w: &WorkerCtx,
    corrupt: bool,
) -> Option<(Value, gg_compute::matmul::ExecutionReport)> {
    let (s, claim) = env
        .post(
            &w.token,
            "/worker/claim",
            json!({ "kinds": ["tensor.matmul.v1"], "wait_secs": 2 }),
        )
        .await;
    if s == 204 {
        return None;
    }
    assert_eq!(s, 200, "{claim}");
    let assignment: Signed<AssignmentV1> =
        serde_json::from_value(claim["assignment"].clone()).unwrap();
    assignment.verify(&env.network, Domain::Assignment).unwrap();
    assert_eq!(assignment.signer, env.server_key);
    assert_eq!(assignment.body.worker_id, w.id);
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
        assert_eq!(Sha256Digest::of_bytes(&bytes).to_hex(), hash);
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
    for (name, f) in &report.outputs {
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
        assert_eq!(s, 200, "upload {name}: {a}");
    }
    Some((claim, report))
}

fn build_submission(
    env: &Env,
    claim: &Value,
    report: &gg_compute::matmul::ExecutionReport,
) -> ResultSubmissionV1 {
    let assignment: Signed<AssignmentV1> =
        serde_json::from_value(claim["assignment"].clone()).unwrap();
    let outputs = report
        .outputs
        .iter()
        .map(|(n, f)| (n.clone(), f.artifact_ref()))
        .collect();
    ResultSubmissionV1 {
        protocol_version: gg_protocol::PROTOCOL_VERSION,
        network: env.network.clone(),
        assignment_hash: assignment
            .body_hash(&env.network, Domain::Assignment)
            .unwrap(),
        task_id: assignment.body.task_id,
        attempt_id: assignment.body.attempt_id,
        spec_hash: assignment.body.spec_hash,
        generation: assignment.body.generation,
        outputs,
        execution_profile: report.engine.to_profile(report.duration_ms),
        declared_metrics: report.metrics.clone(),
        submitted_at: UnixMillis::now(),
    }
}

async fn worker_submit(
    env: &Env,
    w: &WorkerCtx,
    claim: &Value,
    report: &gg_compute::matmul::ExecutionReport,
) -> (u16, Value) {
    let body = build_submission(env, claim, report);
    let signed = Signed::sign(&w.key, &env.network, Domain::Result, body).unwrap();
    post_signed_submission(env, w, &signed).await
}

async fn post_signed_submission(
    env: &Env,
    w: &WorkerCtx,
    signed: &Signed<ResultSubmissionV1>,
) -> (u16, Value) {
    env.post(
        &w.token,
        &format!("/worker/attempts/{}/result", signed.body.attempt_id),
        serde_json::to_value(signed).unwrap(),
    )
    .await
}

struct ValidatorCtx {
    key: SecretKey,
    token: String,
    id: ValidatorId,
    dir: tempfile::TempDir,
}

async fn setup_validator(env: &Env) -> ValidatorCtx {
    let key = SecretKey::generate().unwrap();
    // A worker key cannot log in as a validator.
    let (s, _) = env
        .post(
            "",
            "/validator/challenge",
            json!({ "public_key": key.public_key().to_hex() }),
        )
        .await;
    assert_eq!(s, 404);
    let (s, v) = env
        .post(
            &env.admin,
            "/admin/validators",
            json!({ "public_key": key.public_key().to_hex(), "display_name": "v1" }),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let id: ValidatorId = v["validator_id"].as_str().unwrap().parse().unwrap();
    let token = env.login(&key, "validator-login").await;
    ValidatorCtx {
        key,
        token,
        id,
        dir: tempfile::tempdir().unwrap(),
    }
}

/// Claim one verification, recompute, decide. Returns the decision response.
async fn validator_verify_once(
    env: &Env,
    v: &ValidatorCtx,
) -> Option<(VerificationOutcome, Value)> {
    let (s, work) = env
        .post(&v.token, "/validator/claim", json!({ "wait_secs": 2 }))
        .await;
    if s == 204 {
        return None;
    }
    assert_eq!(s, 200, "{work}");
    let spec: TaskSpecV1 = serde_json::from_value(work["spec"].clone()).unwrap();
    let submission: Signed<ResultSubmissionV1> =
        serde_json::from_value(work["submission"].clone()).unwrap();
    submission.verify(&env.network, Domain::Result).unwrap();
    let task = gg_compute::validate_definition(&spec.definition).unwrap();
    let vid = work["verification_id"].as_str().unwrap().to_owned();
    let mut inputs = BTreeMap::new();
    let mut outputs = BTreeMap::new();
    for (list, map) in [("inputs", &mut inputs), ("outputs", &mut outputs)] {
        for item in work[list].as_array().unwrap() {
            let name = item["name"].as_str().unwrap();
            let hash = item["hash"].as_str().unwrap();
            let (s, bytes) = env
                .get_bytes(&v.token, &format!("/validator/artifacts/{hash}"))
                .await;
            assert_eq!(s, 200, "download {list}/{name}");
            let p = v.dir.path().join(format!("{vid}-{list}-{name}.ggt"));
            std::fs::write(&p, bytes).unwrap();
            map.insert(name.to_owned(), p);
        }
    }
    let verdict =
        gg_compute::matmul::verify(&task, &inputs, &outputs, &submission.body.outputs).unwrap();
    let decision = VerificationDecisionV1 {
        protocol_version: gg_protocol::PROTOCOL_VERSION,
        network: env.network.clone(),
        decision_id: DecisionId::new(),
        submission_hash: submission.body_hash(&env.network, Domain::Result).unwrap(),
        task_id: spec.task_id,
        attempt_id: submission.body.attempt_id,
        validator_id: v.id,
        method: "recompute-reference".into(),
        method_version: 1,
        reference_profile: gg_compute::profiles::REFERENCE_PROFILE.into(),
        checks: verdict.checks,
        artifacts_checked: verdict.artifacts_checked,
        outcome: verdict.outcome,
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
    Some((verdict.outcome, resp))
}

#[tokio::test]
async fn full_vertical_slice_with_receipt_and_single_ledger_write() {
    let env = start(120, 5).await;
    let r = setup_researcher(&env, 100).await;

    // dry run does not persist
    let (a, b) = matmul_inputs(1, 8, 12, 6);
    let ra = upload_input(&env, &r, &a).await;
    let rb = upload_input(&env, &r, &b).await;
    let def = gg_compute::catalog::matmul_definition(8, 12, 6, ra, rb).unwrap();
    let (s, plan) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/jobs", r.project),
            json!({ "name": "dry", "tasks": [def], "dry_run": true }),
        )
        .await;
    assert_eq!(s, 200, "{plan}");
    assert_eq!(plan["dry_run"], true);
    assert!(plan["job_id"].is_null());
    assert_eq!(plan["tasks"][0]["reservation_per_attempt"], "2");

    // wrong reward schedule is refused
    let mut bad = serde_json::to_value(&def).unwrap();
    bad["reward_schedule"]["primary"] = json!("999");
    let (s, e) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/jobs", r.project),
            json!({ "name": "bad", "tasks": [bad] }),
        )
        .await;
    assert_eq!(s, 422, "{e}");

    // unknown kind is rejected explicitly
    let mut unk = serde_json::to_value(&def).unwrap();
    unk["params"] = json!({ "kind": "shell.exec.v1", "cmd": "id" });
    let (s, e) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/jobs", r.project),
            json!({ "name": "bad", "tasks": [unk] }),
        )
        .await;
    assert_eq!(s, 400, "{e}");
    assert!(e["message"].as_str().unwrap().contains("unknown variant"));

    // real submission with idempotency
    let (s, job) = submit_matmul(&env, &r, 2, (16, 32, 8), &[("Idempotency-Key", "k1")]).await;
    assert_eq!(s, 200, "{job}");
    let job_id: JobId = job["job_id"].as_str().unwrap().parse().unwrap();
    let (s, job2) = env
        .post_raw(
            &r.token,
            &format!("/researcher/projects/{}/jobs", r.project),
            json!({ "name": "other", "tasks": [] }),
            &[("Idempotency-Key", "k1")],
        )
        .await;
    assert_eq!(s, 409, "different body with same key must conflict: {job2}");
    let (s, st) = env
        .get(&r.token, &format!("/researcher/jobs/{job_id}"))
        .await;
    assert_eq!(s, 200);
    assert_eq!(st["state"], "queued");
    let task_id: TaskId = st["tasks"][0]["task_id"].as_str().unwrap().parse().unwrap();

    // worker path
    let w = setup_worker(&env).await;
    let (claim, report) = worker_execute(&env, &w, false)
        .await
        .expect("a task should be assigned");
    let (s, p) = env
        .get(&r.token, &format!("/researcher/projects/{}", r.project))
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        p["budget"]["reserved"], "2",
        "budget reserved at assignment: {p}"
    );
    assert_eq!(p["budget"]["available"], "98");
    let attempt_id = claim["assignment"]["body"]["attempt_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (s, hb) = env
        .post(
            &w.token,
            &format!("/worker/attempts/{attempt_id}/heartbeat"),
            json!({}),
        )
        .await;
    assert_eq!(s, 200, "{hb}");

    // a submission signed by another key is refused
    let other = SecretKey::generate().unwrap();
    let body = build_submission(&env, &claim, &report);
    let forged = Signed::sign(&other, &env.network, Domain::Result, body.clone()).unwrap();
    let (s, e) = env
        .post(
            &w.token,
            &format!("/worker/attempts/{attempt_id}/result"),
            serde_json::to_value(&forged).unwrap(),
        )
        .await;
    assert_eq!(s, 403, "{e}");
    // a tampered envelope is refused
    let mut tampered = serde_json::to_value(
        Signed::sign(&w.key, &env.network, Domain::Result, body.clone()).unwrap(),
    )
    .unwrap();
    tampered["body"]["generation"] = json!(7);
    let (s, e) = env
        .post(
            &w.token,
            &format!("/worker/attempts/{attempt_id}/result"),
            tampered,
        )
        .await;
    assert_eq!(s, 403, "{e}");

    let signed_sub = Signed::sign(
        &w.key,
        &env.network,
        Domain::Result,
        build_submission(&env, &claim, &report),
    )
    .unwrap();
    let (s, sub) = post_signed_submission(&env, &w, &signed_sub).await;
    assert_eq!(s, 200, "{sub}");
    assert_eq!(sub["recorded"], true);
    // identical replay: same answer, not recorded twice
    let (s, sub2) = post_signed_submission(&env, &w, &signed_sub).await;
    assert_eq!(s, 200, "{sub2}");
    assert_eq!(sub2["recorded"], false);
    assert_eq!(sub2["submission_hash"], sub["submission_hash"]);
    // a different result for the same attempt is a conflict
    let (s, e) = worker_submit(&env, &w, &claim, &report).await;
    assert_eq!(s, 409, "{e}");
    let (s, st) = env
        .get(&r.token, &format!("/researcher/jobs/{job_id}"))
        .await;
    assert_eq!(s, 200);
    assert_eq!(st["tasks"][0]["state"], "submitted");
    // late heartbeat on a submitted attempt is refused
    let (s, _) = env
        .post(
            &w.token,
            &format!("/worker/attempts/{attempt_id}/heartbeat"),
            json!({}),
        )
        .await;
    assert_eq!(s, 409);

    // validator path
    let v = setup_validator(&env).await;
    let (outcome, resp) = validator_verify_once(&env, &v)
        .await
        .expect("a verification should be pending");
    assert_eq!(outcome, VerificationOutcome::Accepted);
    assert_eq!(resp["applied"], true);
    assert_eq!(resp["task_state"], "accepted");
    let receipt_id = resp["receipt_id"]
        .as_str()
        .expect("receipt issued")
        .to_owned();
    assert!(
        validator_verify_once(&env, &v).await.is_none(),
        "nothing left to verify"
    );

    // researcher sees the result and can download the accepted output
    let (s, detail) = env
        .get(&r.token, &format!("/researcher/tasks/{task_id}"))
        .await;
    assert_eq!(s, 200, "{detail}");
    assert_eq!(detail["task"]["state"], "accepted");
    assert_eq!(detail["receipt_id"], receipt_id);
    let out_hash = detail["accepted_outputs"][0]["hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let (s, bytes) = env
        .get_bytes(
            &r.token,
            &format!("/researcher/projects/{}/artifacts/{out_hash}", r.project),
        )
        .await;
    assert_eq!(s, 200);
    let c = TensorF32::decode(&bytes, &Default::default()).unwrap();
    let (a, b) = matmul_inputs(2, 16, 32, 8);
    let oracle = gg_compute::matmul::matmul_naive_f64(&a, &b);
    assert!(
        gg_compute::compare::compare_f32(
            "c",
            &c,
            &oracle,
            ToleranceV1 {
                abs: 1e-4,
                rel: 1e-4
            }
        )
        .passed
    );
    let (s, st) = env
        .get(&r.token, &format!("/researcher/jobs/{job_id}"))
        .await;
    assert_eq!(s, 200);
    assert_eq!(st["state"], "succeeded");

    // ledger: reserved 2 → worker earned 1 + verification 1; exactly one reward transaction
    let (s, p) = env
        .get(&r.token, &format!("/researcher/projects/{}", r.project))
        .await;
    assert_eq!(s, 200);
    assert_eq!(p["budget"]["available"], "98", "{p}");
    assert_eq!(p["budget"]["reserved"], "0");
    assert_eq!(p["budget"]["spent"], "2");
    let (s, me) = env.get(&w.token, "/worker/me").await;
    assert_eq!(s, 200);
    assert_eq!(me["earned"], "1");
    let (s, receipts) = env.get(&w.token, "/worker/receipts").await;
    assert_eq!(s, 200);
    assert_eq!(receipts.as_array().unwrap().len(), 1);
    let rc = &receipts[0];
    assert_eq!(rc["receipt_id"], receipt_id);
    let signed: Signed<ContributionReceiptV1> =
        serde_json::from_value(rc["receipt"].clone()).unwrap();
    signed.verify(&env.network, Domain::Receipt).unwrap();
    assert_eq!(signed.signer, env.server_key);
    assert_eq!(signed.body.amount, Credits(1));
    assert_eq!(signed.body.worker_public_key, w.key.public_key());
    let canonical = hex::decode(rc["canonical_hex"].as_str().unwrap()).unwrap();
    let from_bytes: Signed<ContributionReceiptV1> =
        Signed::from_canonical_bytes(&canonical).unwrap();
    assert_eq!(from_bytes, signed);
    let mut c = env.tdb.as_ref().unwrap().db.pool().acquire().await.unwrap();
    let (n,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM ledger_transactions WHERE kind = 'reward'")
            .fetch_one(&mut *c)
            .await
            .unwrap();
    assert_eq!(n, 1);
    assert!(
        gg_storage::queries::ledger::unbalanced_transactions(&mut c)
            .await
            .unwrap()
            .is_empty()
    );
    drop(c);

    // a second researcher outside the project cannot see anything
    let (s, r2) = env
        .post(
            &env.admin,
            "/admin/researchers",
            json!({ "display_name": "Eve", "admission": "approved" }),
        )
        .await;
    assert_eq!(s, 200);
    let t2 = r2["token"].as_str().unwrap();
    let (s, _) = env.get(t2, &format!("/researcher/jobs/{job_id}")).await;
    assert_eq!(s, 404);
    let (s, _) = env.get(t2, &format!("/researcher/tasks/{task_id}")).await;
    assert_eq!(s, 404);
    let (s, _) = env
        .get(t2, &format!("/researcher/projects/{}/receipts", r.project))
        .await;
    assert_eq!(s, 404);
    let (s, _) = env
        .get_bytes(
            t2,
            &format!("/researcher/projects/{}/artifacts/{out_hash}", r.project),
        )
        .await;
    assert_eq!(s, 404);
    // pending researcher is refused even with a valid token
    let (s, r3) = env
        .post(
            &env.admin,
            "/admin/researchers",
            json!({ "display_name": "Pat" }),
        )
        .await;
    assert_eq!(s, 200);
    let (s, _) = env
        .get(r3["token"].as_str().unwrap(), "/researcher/me")
        .await;
    assert_eq!(s, 403);
    // admin routes need the admin token
    let (s, _) = env.get(&r.token, "/admin/workers").await;
    assert_eq!(s, 403);
    let (s, _) = env.get("ggt_nope", "/researcher/me").await;
    assert_eq!(s, 401);
    let _ = r.id;
    // The metrics count what happened: one claim served, one decision recorded (accepted).
    let metrics = env
        .http
        .get(format!("{}/v1/admin/metrics", env.base))
        .bearer_auth(&env.admin)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        metrics.contains("gg_claims_assigned_total 1\n"),
        "{metrics}"
    );
    assert!(
        metrics.contains("gg_decisions_total{outcome=\"accepted\"} 1\n"),
        "{metrics}"
    );
    env.stop().await;
}

#[tokio::test]
async fn corrupted_result_is_rejected_and_task_requeued_without_reward() {
    let env = start(120, 5).await;
    let r = setup_researcher(&env, 50).await;
    let (s, job) = submit_matmul(&env, &r, 3, (10, 10, 10), &[]).await;
    assert_eq!(s, 200, "{job}");
    let job_id = job["job_id"].as_str().unwrap().to_owned();
    let w = setup_worker(&env).await;
    let v = setup_validator(&env).await;

    let (claim, report) = worker_execute(&env, &w, true).await.unwrap();
    let (s, _) = worker_submit(&env, &w, &claim, &report).await;
    assert_eq!(s, 200);
    let (outcome, resp) = validator_verify_once(&env, &v).await.unwrap();
    assert_eq!(outcome, VerificationOutcome::Rejected);
    assert_eq!(resp["applied"], true);
    assert_eq!(
        resp["task_state"], "queued",
        "rejected task is re-queued: {resp}"
    );
    assert!(resp["receipt_id"].is_null());
    let (s, me) = env.get(&w.token, "/worker/me").await;
    assert_eq!(s, 200);
    assert_eq!(me["earned"], "0");
    let (s, p) = env
        .get(&r.token, &format!("/researcher/projects/{}", r.project))
        .await;
    assert_eq!(s, 200);
    assert_eq!(p["budget"]["available"], "50", "reservation released: {p}");
    assert_eq!(p["budget"]["spent"], "0");

    // the rejected worker is never offered the same task again; another worker gets generation 2
    assert!(
        worker_execute(&env, &w, false).await.is_none(),
        "rejected worker must not reclaim the task"
    );
    let w2 = setup_worker(&env).await;
    let (claim2, report2) = worker_execute(&env, &w2, false).await.unwrap();
    assert_eq!(claim2["assignment"]["body"]["generation"], 2);
    let (s, _) = worker_submit(&env, &w2, &claim2, &report2).await;
    assert_eq!(s, 200);
    let (outcome, resp) = validator_verify_once(&env, &v).await.unwrap();
    assert_eq!(outcome, VerificationOutcome::Accepted);
    assert!(resp["receipt_id"].is_string());
    let (s, st) = env
        .get(&r.token, &format!("/researcher/jobs/{job_id}"))
        .await;
    assert_eq!(s, 200);
    assert_eq!(st["state"], "succeeded");
    assert_eq!(st["tasks"][0]["attempts_made"], 2);
    let (s, receipts) = env.get(&w.token, "/worker/receipts").await;
    assert_eq!(s, 200);
    assert!(
        receipts.as_array().unwrap().is_empty(),
        "the dishonest worker earned nothing"
    );
    let (s, receipts) = env.get(&w2.token, "/worker/receipts").await;
    assert_eq!(s, 200);
    assert_eq!(
        receipts.as_array().unwrap().len(),
        1,
        "one contribution only"
    );
    env.stop().await;
}

#[tokio::test]
async fn expired_lease_is_reaped_and_late_submission_refused() {
    let env = start(1, 1).await; // 1 s leases, reaper every second
    let r = setup_researcher(&env, 50).await;
    let (s, job) = submit_matmul(&env, &r, 4, (4, 4, 4), &[]).await;
    assert_eq!(s, 200, "{job}");
    let w = setup_worker(&env).await;
    let (claim, report) = worker_execute(&env, &w, false).await.unwrap();
    let attempt_id = claim["assignment"]["body"]["attempt_id"]
        .as_str()
        .unwrap()
        .to_owned();
    // wait for expiry + reaper
    tokio::time::sleep(std::time::Duration::from_millis(3500)).await;
    let (s, e) = worker_submit(&env, &w, &claim, &report).await;
    assert_eq!(s, 409, "late submission must be refused: {e}");
    let (s, e) = env
        .post(
            &w.token,
            &format!("/worker/attempts/{attempt_id}/heartbeat"),
            json!({}),
        )
        .await;
    assert_eq!(s, 409, "{e}");
    let (s, p) = env
        .get(&r.token, &format!("/researcher/projects/{}", r.project))
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        p["budget"]["reserved"], "0",
        "reservation released by the reaper: {p}"
    );
    // task was re-queued with a new generation and can be claimed again
    let (claim2, _) = worker_execute(&env, &w, false)
        .await
        .expect("re-queued task");
    assert_eq!(claim2["assignment"]["body"]["generation"], 2);
    env.stop().await;
}

#[tokio::test]
async fn cancelled_job_receives_no_assignment_and_releases_budget() {
    let env = start(120, 5).await;
    let r = setup_researcher(&env, 50).await;
    let (s, job) = submit_matmul(&env, &r, 5, (4, 4, 4), &[]).await;
    assert_eq!(s, 200, "{job}");
    let job_id = job["job_id"].as_str().unwrap().to_owned();
    let w = setup_worker(&env).await;
    let (claim, report) = worker_execute(&env, &w, false).await.unwrap();
    let (s, c) = env
        .post(
            &r.token,
            &format!("/researcher/jobs/{job_id}/cancel"),
            json!({}),
        )
        .await;
    assert_eq!(s, 200, "{c}");
    assert_eq!(c["tasks_cancelled"], 1);
    let (s, e) = worker_submit(&env, &w, &claim, &report).await;
    assert_eq!(s, 409, "submission after cancellation: {e}");
    assert!(
        worker_execute(&env, &w, false).await.is_none(),
        "cancelled task is never assigned"
    );
    let (s, p) = env
        .get(&r.token, &format!("/researcher/projects/{}", r.project))
        .await;
    assert_eq!(s, 200);
    assert_eq!(p["budget"]["available"], "50", "{p}");
    assert_eq!(p["budget"]["reserved"], "0");
    let (s, st) = env
        .get(&r.token, &format!("/researcher/jobs/{job_id}"))
        .await;
    assert_eq!(s, 200);
    assert_eq!(st["state"], "cancelled");
    // idempotent cancel
    let (s, c) = env
        .post(
            &r.token,
            &format!("/researcher/jobs/{job_id}/cancel"),
            json!({}),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(c["tasks_cancelled"], 0);
    env.stop().await;
}

#[tokio::test]
async fn budget_gates_assignment() {
    let env = start(120, 5).await;
    let r = setup_researcher(&env, 1).await; // reservation needs 2
    let (s, job) = submit_matmul(&env, &r, 6, (4, 4, 4), &[]).await;
    assert_eq!(s, 200, "{job}");
    let w = setup_worker(&env).await;
    assert!(
        worker_execute(&env, &w, false).await.is_none(),
        "no assignment without budget"
    );
    let (s, _) = env
        .post(
            &env.admin,
            &format!("/admin/projects/{}/grant", r.project),
            json!({ "amount": "1" }),
        )
        .await;
    assert_eq!(s, 200);
    assert!(worker_execute(&env, &w, false).await.is_some());
    env.stop().await;
}

#[tokio::test]
async fn openapi_and_health() {
    let env = start(120, 5).await;
    let (s, h) = env.get("", "/health").await;
    assert_eq!(s, 200);
    assert_eq!(h["database"], true);
    let (s, doc) = env.get("", "/openapi.json").await;
    assert_eq!(s, 200);
    assert!(
        doc["paths"]["/v1/worker/claim"].is_object() || doc["paths"]["/worker/claim"].is_object(),
        "{}",
        doc["paths"]
    );
    let (s, info) = env.get("", "/info").await;
    assert_eq!(s, 200);
    assert_eq!(info["server_public_key"], env.server_key.to_hex());
    env.stop().await;
}

#[tokio::test]
async fn the_operator_profile_is_published_before_anything_is_submitted() {
    let env = start(120, 5).await;
    let (s, profile) = env.get("", "/catalog/ops").await;
    assert_eq!(s, 200, "{profile}");
    assert_eq!(profile["profile"], gg_compute::graph::PROFILE_VERSION);
    assert_eq!(profile["format"], gg_compute::graph::FORMAT);
    let ops = profile["operators"].as_array().expect("operator list");
    assert_eq!(ops.len(), gg_compute::graph::profile().len());
    for name in ["CausalMask", "Slice", "ReduceMean", "Rsqrt"] {
        assert!(
            ops.iter().any(|o| o["name"] == name),
            "the profile publishes {name}"
        );
    }
    // A researcher must be able to decide before submitting: name, ONNX equivalent, attributes, bounds.
    let matmul = ops
        .iter()
        .find(|o| o["name"] == "MatMul")
        .expect("MatMul published");
    assert!(
        matmul["onnx"]
            .as_str()
            .unwrap_or_default()
            .contains("opset")
    );
    assert_eq!(matmul["min_inputs"], 2);
    let conv = ops
        .iter()
        .find(|o| o["name"] == "Conv2d")
        .expect("Conv2d published");
    let attrs = conv["attributes"].as_array().expect("attributes");
    assert!(attrs.iter().any(|a| a == "padding"));
    assert!(profile["max_parameters"].as_u64().unwrap_or(0) > 0);
    // What the server publishes is exactly what local validation runs against.
    for spec in gg_compute::graph::profile() {
        assert!(
            ops.iter().any(|o| o["name"] == spec.name),
            "{} is missing from the response",
            spec.name
        );
    }
    env.stop().await;
}

#[tokio::test]
async fn a_project_exists_only_once_the_data_rule_is_accepted() {
    let env = start(120, 5).await;
    // The rule is published before anything is submitted, in plain words.
    let (s, info) = env.get("", "/info").await;
    assert_eq!(s, 200, "{info}");
    assert_eq!(info["data_policy"], "public-synthetic-only");
    let text = info["data_policy_text"].as_str().unwrap_or_default();
    assert!(
        text.contains("synthetic") && text.contains("sees the data"),
        "the rule must say what a contributor can see: {text}"
    );

    let (s, r) = env
        .post(
            &env.admin,
            "/admin/researchers",
            json!({ "display_name": "Ada", "admission": "approved" }),
        )
        .await;
    assert_eq!(s, 200, "{r}");
    let id: ResearcherId = r["researcher_id"].as_str().unwrap().parse().unwrap();

    // Without the acknowledgement, no project, and the refusal explains the rule.
    let (s, e) = env
        .post(
            &env.admin,
            "/admin/projects",
            json!({ "name": "p", "allowed_kinds": ["tensor.matmul.v1"], "owner_researcher_id": id }),
        )
        .await;
    assert_eq!(s, 422, "{e}");
    let message = e["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("public-synthetic-only") && message.contains("synthetic"),
        "{message}"
    );

    // With it, the project records which rule it accepted.
    let (s, p) = env
        .post(
            &env.admin,
            "/admin/projects",
            json!({ "name": "p", "allowed_kinds": ["tensor.matmul.v1"], "owner_researcher_id": id, "data_policy_ack": true }),
        )
        .await;
    assert_eq!(s, 200, "{p}");
    assert_eq!(p["data_policy"], "public-synthetic-only");
    env.stop().await;
}

/// The work a lease covers costs no quota: under a budget of 5 requests, a worker downloads its
/// inputs, heartbeats, uploads and submits dozens of times over without a single 429. What its
/// lease does not cover is still charged, and refused once the budget is spent.
#[tokio::test]
async fn requests_covered_by_a_lease_do_not_consume_the_quota() {
    let env = start_with(60, 1, |c| {
        c.quota_requests_per_minute = 60;
        c.quota_burst = 5;
        c.quota_compute_multiplier = 1;
    })
    .await;
    let r = setup_researcher(&env, 100).await;
    let w = setup_worker(&env).await;
    let (s, job) = submit_matmul(&env, &r, 7, (16, 16, 16), &[]).await;
    assert_eq!(s, 200, "{job}");

    // Claim (charged), download, execute and upload (covered).
    let (claim, report) = worker_execute(&env, &w, false)
        .await
        .expect("a task to claim");
    let attempt = claim["assignment"]["body"]["attempt_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let inputs: Vec<String> = claim["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["hash"].as_str().unwrap().to_owned())
        .collect();
    for round in 0..20 {
        for hash in &inputs {
            let (s, _) = env
                .get_bytes(&w.token, &format!("/worker/artifacts/{hash}"))
                .await;
            assert_eq!(s, 200, "download {round} of an input of the live attempt");
        }
        let (s, hb) = env
            .post(
                &w.token,
                &format!("/worker/attempts/{attempt}/heartbeat"),
                json!({}),
            )
            .await;
        assert_eq!(s, 200, "heartbeat {round}: {hb}");
    }
    let (s, sub) = worker_submit(&env, &w, &claim, &report).await;
    assert_eq!(s, 200, "{sub}");
    assert_eq!(sub["recorded"], true);

    // The attempt is submitted: its inputs are no longer covered. Each request is charged, and
    // the budget runs out within a few requests.
    let mut statuses = Vec::new();
    for _ in 0..10 {
        let (s, _) = env
            .get_bytes(&w.token, &format!("/worker/artifacts/{}", inputs[0]))
            .await;
        statuses.push(s);
        if s == 429 {
            break;
        }
    }
    assert_eq!(statuses.last(), Some(&429), "{statuses:?}");
    assert!(
        statuses.iter().all(|&s| s == 404 || s == 429),
        "{statuses:?}"
    );
    assert!(
        statuses.len() <= 6,
        "charged from the first uncovered request: {statuses:?}"
    );
    env.stop().await;
}

/// An operator withdraws a dataset published by mistake (INCIDENTS.md): the job that uses it is
/// cancelled with its reservation released, even while a worker holds one of its tasks; the bytes
/// are gone; the hash is neither served nor accepted again. Withdrawing twice changes nothing.
#[tokio::test]
async fn a_withdrawn_artifact_cancels_what_uses_it_and_is_never_served_again() {
    let env = start(60, 1).await;
    let r = setup_researcher(&env, 100).await;
    let w = setup_worker(&env).await;
    let (a, b) = matmul_inputs(21, 16, 16, 16);
    let ra = upload_input(&env, &r, &a).await;
    let rb = upload_input(&env, &r, &b).await;
    let def = gg_compute::catalog::matmul_definition(16, 16, 16, ra, rb).unwrap();
    let (s, job) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/jobs", r.project),
            json!({ "name": "mm", "tasks": [def.clone(), def] }),
        )
        .await;
    assert_eq!(s, 200, "{job}");
    // A worker holds one of the two tasks: a reservation is open.
    let (s, claim) = env
        .post(
            &w.token,
            "/worker/claim",
            json!({ "kinds": ["tensor.matmul.v1"], "wait_secs": 2 }),
        )
        .await;
    assert_eq!(s, 200, "{claim}");
    let (_, p) = env
        .get(&r.token, &format!("/researcher/projects/{}", r.project))
        .await;
    assert_ne!(p["budget"]["reserved"], "0", "{p}");

    let (s, out) = env
        .post(
            &env.admin,
            &format!("/admin/artifacts/{}/withdraw", ra.hash),
            json!({ "reason": "published by mistake" }),
        )
        .await;
    assert_eq!(s, 200, "{out}");
    assert_eq!(out["jobs_cancelled"], 1, "{out}");
    assert_eq!(out["tasks_cancelled"], 2, "{out}");
    assert_eq!(out["bytes_deleted"], true, "{out}");

    // The reservation is released and nothing is left to claim.
    let (_, p) = env
        .get(&r.token, &format!("/researcher/projects/{}", r.project))
        .await;
    assert_eq!(p["budget"]["reserved"], "0", "{p}");
    assert_eq!(p["budget"]["available"], "100", "{p}");
    let (s, _) = env
        .post(
            &w.token,
            "/worker/claim",
            json!({ "kinds": ["tensor.matmul.v1"], "wait_secs": 0 }),
        )
        .await;
    assert_eq!(s, 204);
    // Neither the researcher nor the worker that held the task can download it.
    let (s, _) = env
        .get_bytes(
            &r.token,
            &format!("/researcher/projects/{}/artifacts/{}", r.project, ra.hash),
        )
        .await;
    assert_eq!(s, 404);
    let (s, _) = env
        .get_bytes(&w.token, &format!("/worker/artifacts/{}", ra.hash))
        .await;
    assert_eq!(s, 404);
    // The same bytes cannot be stored again.
    let bytes = a.encode();
    let (s, _) = env
        .put_bytes(
            &r.token,
            &format!(
                "/researcher/projects/{}/artifacts/{}?size={}&schema=ggt1-f32",
                r.project,
                ra.hash,
                bytes.len()
            ),
            bytes,
        )
        .await;
    assert_eq!(s, 409);
    // The other input is untouched.
    let (s, _) = env
        .get_bytes(
            &r.token,
            &format!("/researcher/projects/{}/artifacts/{}", r.project, rb.hash),
        )
        .await;
    assert_eq!(s, 200);
    // Withdrawing again is harmless.
    let (s, again) = env
        .post(
            &env.admin,
            &format!("/admin/artifacts/{}/withdraw", ra.hash),
            json!({ "reason": "published by mistake" }),
        )
        .await;
    assert_eq!(s, 200, "{again}");
    assert_eq!(again["jobs_cancelled"], 0);
    assert_eq!(again["bytes_deleted"], false);
    env.stop().await;
}

/// A research deployment that does not bill its users: every new project starts with the
/// configured grant, so a researcher submits without asking for credits, and the ledger still
/// records the grant like any other.
#[tokio::test]
async fn a_new_project_receives_the_configured_initial_grant() {
    let env = start_with(60, 1, |c| c.project_initial_grant = 1_000_000_000).await;
    let (s, researcher) = env
        .post(
            &env.admin,
            "/admin/researchers",
            json!({ "display_name": "Ada", "admission": "approved" }),
        )
        .await;
    assert_eq!(s, 200, "{researcher}");
    let (s, p) = env
        .post(
            &env.admin,
            "/admin/projects",
            json!({ "name": "lab", "allowed_kinds": ["tensor.matmul.v1"],
                    "owner_researcher_id": researcher["researcher_id"], "data_policy_ack": true }),
        )
        .await;
    assert_eq!(s, 200, "{p}");
    let token = researcher["token"].as_str().unwrap();
    let (s, view) = env
        .get(
            token,
            &format!("/researcher/projects/{}", p["project_id"].as_str().unwrap()),
        )
        .await;
    assert_eq!(s, 200, "{view}");
    assert_eq!(view["budget"]["available"], "1000000000", "{view}");
    assert_eq!(view["budget"]["reserved"], "0");
    env.stop().await;
}

/// With a minimum worker version configured, an older or silent worker gets no task (426); a
/// current one is served as usual.
#[tokio::test]
async fn a_worker_below_the_minimum_version_gets_no_task() {
    let env = start_with(60, 1, |c| c.min_worker_version = "0.2.0".into()).await;
    let r = setup_researcher(&env, 100).await;
    let w = setup_worker(&env).await;
    let (s, _) = submit_matmul(&env, &r, 3, (16, 16, 16), &[]).await;
    assert_eq!(s, 200);
    for version in [None, Some("0.1.9")] {
        let mut body = json!({ "kinds": ["tensor.matmul.v1"], "wait_secs": 0 });
        if let Some(v) = version {
            body["agent_version"] = json!(v);
        }
        let (s, e) = env.post(&w.token, "/worker/claim", body).await;
        assert_eq!(s, 426, "{version:?}: {e}");
        assert!(e["message"].as_str().unwrap().contains("0.2.0"), "{e}");
    }
    let (s, claim) = env
        .post(
            &w.token,
            "/worker/claim",
            json!({ "kinds": ["tensor.matmul.v1"], "wait_secs": 0, "agent_version": "0.2.0" }),
        )
        .await;
    assert_eq!(s, 200, "{claim}");
    env.stop().await;
}

/// A worker that announces a 5 s heartbeat gets a 20 s lease, renewed at 20 s: if it vanishes, its
/// task is free again within seconds. Without an announced rhythm, the configured length applies.
#[tokio::test]
async fn the_lease_follows_the_heartbeat_rhythm_the_worker_announces() {
    let env = start(60, 1).await;
    let r = setup_researcher(&env, 100).await;
    let w = setup_worker(&env).await;
    for seed in [5, 6] {
        let (s, _) = submit_matmul(&env, &r, seed, (16, 16, 16), &[]).await;
        assert_eq!(s, 200);
    }
    let lease_secs = |attempt: String| {
        let tdb = env.tdb.as_ref().unwrap();
        async move {
            let mut c = tdb.db.pool().acquire().await.unwrap();
            sqlx::query_scalar::<_, f64>(
                "SELECT extract(epoch from (expires_at - now()))::float8 FROM leases WHERE attempt_id = $1::uuid",
            )
            .bind(attempt)
            .fetch_one(&mut *c)
            .await
            .unwrap()
        }
    };
    let (s, claim) = env
        .post(
            &w.token,
            "/worker/claim",
            json!({ "kinds": ["tensor.matmul.v1"], "wait_secs": 0, "heartbeat_secs": 5 }),
        )
        .await;
    assert_eq!(s, 200, "{claim}");
    let a = claim["assignment"]["body"]["attempt_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let left = lease_secs(a.clone()).await;
    assert!((17.0..=20.5).contains(&left), "20 s lease, got {left}");
    let (s, _) = env
        .post(
            &w.token,
            &format!("/worker/attempts/{a}/heartbeat"),
            json!({}),
        )
        .await;
    assert_eq!(s, 200);
    let left = lease_secs(a).await;
    assert!((17.0..=20.5).contains(&left), "renewed at 20 s, got {left}");
    // No announced rhythm: the configured 60 s.
    let (s, claim) = env
        .post(
            &w.token,
            "/worker/claim",
            json!({ "kinds": ["tensor.matmul.v1"], "wait_secs": 0 }),
        )
        .await;
    assert_eq!(s, 200, "{claim}");
    let a = claim["assignment"]["body"]["attempt_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let left = lease_secs(a).await;
    assert!((55.0..=60.5).contains(&left), "configured 60 s, got {left}");
    env.stop().await;
}

/// ADR-0037: a validator learns the inputs of a task being computed and may read them before it
/// holds any verification, so that it downloads them while the worker computes; once the task is
/// decided, they are neither announced nor readable any more.
#[tokio::test]
async fn a_validator_may_prefetch_the_inputs_of_tasks_being_computed() {
    let env = start(120, 5).await;
    let r = setup_researcher(&env, 100).await;
    let w = setup_worker(&env).await;
    let v = setup_validator(&env).await;
    let (s, job) = submit_matmul(&env, &r, 3, (8, 12, 6), &[]).await;
    assert_eq!(s, 200, "{job}");
    let upcoming = |profile: &'static str| {
        let (env, v) = (&env, &v);
        async move {
            let (s, u) = env
                .post(
                    &v.token,
                    "/validator/upcoming",
                    json!({ "reference_profile": profile }),
                )
                .await;
            assert_eq!(s, 200, "{u}");
            u["artifacts"].as_array().unwrap().clone()
        }
    };
    let cpu = gg_compute::profiles::REFERENCE_PROFILE;
    // Queued, not being computed: nothing announced, nothing readable.
    assert!(upcoming(cpu).await.is_empty());
    let (claim, report) = worker_execute(&env, &w, false).await.expect("a task");
    let spec: TaskSpecV1 = serde_json::from_value(claim["spec"].clone()).unwrap();
    let a = spec.definition.inputs["a"];
    // Being computed: both inputs announced, largest first, and readable.
    let announced = upcoming(cpu).await;
    assert_eq!(announced.len(), 2, "{announced:?}");
    assert!(announced[0]["size"].as_u64() >= announced[1]["size"].as_u64());
    assert!(announced.iter().any(|x| x["hash"] == a.hash.to_hex()));
    let (s, bytes) = env
        .get_bytes(&v.token, &format!("/validator/artifacts/{}", a.hash))
        .await;
    assert_eq!(s, 200);
    assert_eq!(bytes.len() as u64, a.size);
    // A validator whose profile may not verify this kind is not told about it.
    assert!(
        upcoming(gg_compute::profiles::GPU_F32_LIBTORCH_CUDA_V1)
            .await
            .is_empty()
    );
    // Submitted: still ahead of the validator.
    let (s, _) = worker_submit(&env, &w, &claim, &report).await;
    assert_eq!(s, 200);
    assert_eq!(upcoming(cpu).await.len(), 2);
    // Decided: gone, and no longer readable.
    let (outcome, _) = validator_verify_once(&env, &v).await.expect("work");
    assert_eq!(outcome, VerificationOutcome::Accepted);
    assert!(upcoming(cpu).await.is_empty());
    let (s, _) = env
        .get_bytes(&v.token, &format!("/validator/artifacts/{}", a.hash))
        .await;
    assert_eq!(s, 404);
}
