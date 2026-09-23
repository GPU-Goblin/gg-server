//! Collective training end to end (ADR-0011) over HTTP against a real PostgreSQL: a run of
//! `mlp.grad.v1` rounds executed by two in-process workers and one validator, aggregated by the
//! server coordinator, then compared with the single-process reference training. Also: a
//! cancelled round is reissued with the same checkpoint and late results of the cancelled job are
//! refused, never integrated.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gg_compute::engine::EngineSelection;
use gg_compute::grad::{CoordOptim, reference_training};
use gg_compute::mlp::{MlpTask, Optim, Weights, initial_weights};
use gg_compute::synthetic::SyntheticMlpConfig;
use gg_control::config::ServerConfig;
use gg_control::server::{Running, serve};
use gg_control::state::AppState;
use gg_core::*;
use gg_protocol::domain::Domain;
use gg_protocol::keys::SecretKey;
use gg_protocol::messages::*;
use gg_protocol::signed::Signed;
use gg_protocol::tensor_file::TensorF32;
use gg_storage::queries::identities;
use gg_storage::testing::TestDb;
use serde_json::{Value, json};

struct Env {
    /// The server's state, to act as a restart would (the memory kept between rounds, ADR-0036).
    state: AppState,
    tdb: Option<TestDb>,
    running: Option<Running>,
    http: reqwest::Client,
    base: String,
    admin: String,
    network: NetworkId,
    _tmp: tempfile::TempDir,
}

async fn start() -> Env {
    start_with(120).await
}

/// `pending_output_secs`: time a producer has to send the durable copy of an activation it
/// declared before (ADR-0026).
async fn start_with(pending_output_secs: i64) -> Env {
    start_configured(|cfg| cfg.pending_output_secs = pending_output_secs).await
}

/// A server whose configuration `configure` adjusts after the test defaults.
async fn start_configured(configure: impl FnOnce(&mut ServerConfig)) -> Env {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
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
        ..ServerConfig::default()
    };
    let mut cfg = cfg;
    configure(&mut cfg);
    let state = AppState::new(cfg, tdb.db.clone()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let running = serve(state.clone(), listener).await.unwrap();
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
        state,
        base: running.url(),
        running: Some(running),
        tdb: Some(tdb),
        http: reqwest::Client::new(),
        admin,
        network: NetworkId::new("gg-e2e").unwrap(),
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
        .post(
            &env.admin,
            "/admin/projects",
            json!({ "name": "demo", "allowed_kinds": ["mlp.grad.v1", "graph.grad.v1", "graph.stage.v1"], "owner_researcher_id": id, "data_policy_ack": true }),
        )
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

async fn upload_bytes(env: &Env, r: &Researcher, bytes: Vec<u8>, schema: &str) -> ArtifactRef {
    let hash = Sha256Digest::of_bytes(&bytes);
    let size = bytes.len() as u64;
    let (s, a) = env
        .put_bytes(
            &r.token,
            &format!(
                "/researcher/projects/{}/artifacts/{hash}?size={size}&schema={schema}",
                r.project
            ),
            bytes,
        )
        .await;
    assert_eq!(s, 200, "{a}");
    ArtifactRef { hash, size }
}

struct WorkerCtx {
    key: SecretKey,
    token: String,
    dir: tempfile::TempDir,
    /// Accelerator memory announced when claiming (C1).
    device: u64,
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
        dir: tempfile::tempdir().unwrap(),
        device: 64 << 30,
    }
}

struct Executed {
    submission: Signed<ResultSubmissionV1>,
    /// Hashes of the inputs the task read.
    inputs: Vec<String>,
    /// Activations declared instead of uploaded: `(upload path, bytes)` of their durable copy.
    deferred: Vec<(String, Vec<u8>, Vec<Sha256Digest>)>,
}

/// How the test worker hands its outputs over.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Uploads everything before submitting.
    Honest,
    /// Alters its first activation before upload.
    Corrupt,
    /// Declares its activations (ADR-0026) and keeps their durable copy for later.
    Declare,
}

/// Claim one `mlp.grad.v1` task, execute it on CPU and upload the outputs; the signed submission
/// is returned without being posted, so tests decide when (or whether) it reaches the server.
async fn worker_execute(env: &Env, w: &WorkerCtx) -> Option<Executed> {
    worker_execute_with(env, w, Mode::Honest).await
}

/// Same, and when `corrupt` is set, a dishonest worker: the first activation it produces
/// (`out.*`) is altered before upload, as a signed and well-formed but false result.
async fn worker_execute_with(env: &Env, w: &WorkerCtx, mode: Mode) -> Option<Executed> {
    let (s, claim) = env
        .post(
            &w.token,
            "/worker/claim",
            // Like the real worker (C1): it announces its memory and its profile.
            json!({
                "kinds": ["mlp.grad.v1", "graph.grad.v1", "graph.stage.v1"], "wait_secs": 0,
                "ram_bytes": 64u64 << 30, "device_memory_bytes": w.device,
                "profile": gg_compute::graph::PROFILE_VERSION,
            }),
        )
        .await;
    if s == 204 {
        return None;
    }
    assert_eq!(s, 200, "{claim}");
    let assignment: Signed<AssignmentV1> =
        serde_json::from_value(claim["assignment"].clone()).unwrap();
    assignment.verify(&env.network, Domain::Assignment).unwrap();
    let spec: TaskSpecV1 = serde_json::from_value(claim["spec"].clone()).unwrap();
    let task = gg_compute::validate_definition(&spec.definition).unwrap();
    let mut inputs = BTreeMap::new();
    let mut input_hashes = Vec::new();
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
        input_hashes.push(hash.to_owned());
    }
    let out_dir = w
        .dir
        .path()
        .join(format!("out-{}", assignment.body.attempt_id));
    let mut report =
        gg_compute::execute_task(EngineSelection::FlexCpu, &task, &inputs, &out_dir).unwrap();
    if mode == Mode::Corrupt
        && let Some((_, f)) = report
            .outputs
            .iter_mut()
            .find(|(n, _)| n.starts_with("out."))
    {
        // The tensor's last float, plus 1: the file stays well formed.
        let mut bytes = std::fs::read(&f.path).unwrap();
        let n = bytes.len();
        let v = f32::from_le_bytes(bytes[n - 4..].try_into().unwrap()) + 1.0;
        bytes[n - 4..].copy_from_slice(&v.to_le_bytes());
        std::fs::write(&f.path, &bytes).unwrap();
        f.hash = Sha256Digest::of_bytes(&bytes);
        f.size = n as u64;
    }
    let mut deferred = Vec::new();
    for (name, f) in &report.outputs {
        let bytes = std::fs::read(&f.path).unwrap();
        if mode == Mode::Declare && (name.starts_with("out.") || name.starts_with("din.")) {
            let (s, info) = env.get("", "/info").await;
            assert_eq!(s, 200, "{info}");
            let chunk = info["p2p_chunk_bytes"].as_u64().unwrap() as usize;
            let digests: Vec<Sha256Digest> =
                bytes.chunks(chunk).map(Sha256Digest::of_bytes).collect();
            let (s, d) = env
                .post(
                    &w.token,
                    &format!(
                        "/worker/attempts/{}/outputs/{}/declare",
                        assignment.body.attempt_id, f.hash
                    ),
                    json!({ "size": f.size, "chunks": digests }),
                )
                .await;
            assert_eq!(s, 200, "declare {name}: {d}");
            deferred.push((
                format!(
                    "/worker/attempts/{}/outputs/{}?size={}&schema=ggt1-f32",
                    assignment.body.attempt_id, f.hash, f.size
                ),
                bytes,
                digests,
            ));
            continue;
        }
        let schema = if name == "grad_meta" || name == "stage_meta" {
            "json"
        } else {
            "ggt1-f32"
        };
        let (s, a) = env
            .put_bytes(
                &w.token,
                &format!(
                    "/worker/attempts/{}/outputs/{}?size={}&schema={schema}",
                    assignment.body.attempt_id, f.hash, f.size
                ),
                bytes,
            )
            .await;
        assert_eq!(s, 200, "upload {name}: {a}");
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
    let submission = Signed::sign(&w.key, &env.network, Domain::Result, body).unwrap();
    Some(Executed {
        submission,
        inputs: input_hashes,
        deferred,
    })
}

async fn worker_submit(env: &Env, w: &WorkerCtx, e: &Executed) -> (u16, Value) {
    env.post(
        &w.token,
        &format!("/worker/attempts/{}/result", e.submission.body.attempt_id),
        serde_json::to_value(&e.submission).unwrap(),
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

/// Claim one verification, recompute with the reference profile, decide.
async fn validator_verify_once(env: &Env, v: &ValidatorCtx) -> Option<VerificationOutcome> {
    let (s, resp, outcome) = validator_verify_as(env, v, None, REFERENCE_PROFILE, None).await?;
    assert_eq!(s, 200, "{resp}");
    Some(outcome)
}

use gg_compute::profiles::{GPU_F32_WGPU_VULKAN_V1, REFERENCE_PROFILE};

/// Claim one verification announcing `claim_profile` (absent: the CPU reference), recompute on the
/// CPU, and post a decision that states `decision_profile`, with `outcome` in place of the
/// recomputed one if given. Returns the decision's HTTP status and answer, and its outcome.
async fn validator_verify_as(
    env: &Env,
    v: &ValidatorCtx,
    claim_profile: Option<&str>,
    decision_profile: &str,
    outcome: Option<VerificationOutcome>,
) -> Option<(u16, Value, VerificationOutcome)> {
    let work = claim_work(env, v, claim_profile).await?;
    Some(decide_work(env, v, &work, decision_profile, outcome).await)
}

/// Claim one verification announcing `claim_profile`; `None` when nothing is offered.
async fn claim_work(env: &Env, v: &ValidatorCtx, claim_profile: Option<&str>) -> Option<Value> {
    let (s, work) = env
        .post(
            &v.token,
            "/validator/claim",
            json!({ "wait_secs": 0, "reference_profile": claim_profile }),
        )
        .await;
    if s == 204 {
        return None;
    }
    assert_eq!(s, 200, "{work}");
    Some(work)
}

/// Recompute a claimed verification on the CPU and post a decision stating `decision_profile`.
async fn decide_work(
    env: &Env,
    v: &ValidatorCtx,
    work: &Value,
    decision_profile: &str,
    outcome: Option<VerificationOutcome>,
) -> (u16, Value, VerificationOutcome) {
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
    let decision = VerificationDecisionV1 {
        protocol_version: gg_protocol::PROTOCOL_VERSION,
        network: env.network.clone(),
        decision_id: DecisionId::new(),
        submission_hash: submission.body_hash(&env.network, Domain::Result).unwrap(),
        task_id: spec.task_id,
        attempt_id: submission.body.attempt_id,
        validator_id: v.id,
        method: "recompute-reference".into(),
        method_version: spec.definition.validation_policy.method_version,
        reference_profile: decision_profile.into(),
        checks: verdict.checks,
        artifacts_checked: verdict.artifacts_checked,
        outcome: outcome.unwrap_or(verdict.outcome),
        decided_at: UnixMillis::now(),
    };
    let outcome = decision.outcome;
    let signed = Signed::sign(&v.key, &env.network, Domain::Verification, decision).unwrap();
    let (s, resp) = env
        .post(
            &v.token,
            &format!("/validator/verifications/{vid}/decision"),
            serde_json::to_value(&signed).unwrap(),
        )
        .await;
    (s, resp, outcome)
}

fn dataset() -> (TensorF32, TensorF32) {
    let ds = SyntheticMlpConfig {
        d_input: 6,
        d_output: 2,
        d_teacher: 8,
        n_train: 512,
        n_eval: 16,
        seed: 21,
        noise: 0.02,
    }
    .generate()
    .unwrap();
    (ds.train_x, ds.train_y)
}

async fn download_weights(env: &Env, r: &Researcher, refs: &Value) -> Weights {
    let mut got: BTreeMap<&str, TensorF32> = BTreeMap::new();
    for name in ["w1", "b1", "w2", "b2"] {
        let hash = refs[name]["hash"]
            .as_str()
            .unwrap_or_else(|| panic!("checkpoint lacks {name}: {refs}"));
        let (s, bytes) = env
            .get_bytes(
                &r.token,
                &format!("/researcher/projects/{}/artifacts/{hash}", r.project),
            )
            .await;
        assert_eq!(s, 200, "download {name}");
        assert_eq!(Sha256Digest::of_bytes(&bytes).to_hex(), hash);
        got.insert(
            name,
            TensorF32::decode(&bytes, &Default::default()).unwrap(),
        );
    }
    Weights {
        w1: got["w1"].clone(),
        b1: got["b1"].clone(),
        w2: got["w2"].clone(),
        b2: got["b2"].clone(),
    }
}

/// Drive workers and the validator until the run reaches a terminal state.
async fn drive(
    env: &Env,
    r: &Researcher,
    run: &str,
    workers: &[WorkerCtx],
    v: &ValidatorCtx,
    deadline: Duration,
) -> (Value, usize, usize) {
    let started = Instant::now();
    let mut submitted = 0;
    let mut decided = 0;
    loop {
        assert!(started.elapsed() < deadline, "run did not finish in time");
        let mut progressed = false;
        for w in workers {
            if let Some(e) = worker_execute(env, w).await {
                let (s, resp) = worker_submit(env, w, &e).await;
                assert_eq!(s, 200, "{resp}");
                submitted += 1;
                progressed = true;
            }
        }
        while let Some(outcome) = validator_verify_once(env, v).await {
            assert_eq!(outcome, VerificationOutcome::Accepted);
            decided += 1;
            progressed = true;
        }
        let (s, view) = env
            .get(&r.token, &format!("/researcher/collective/{run}"))
            .await;
        assert_eq!(s, 200, "{view}");
        if matches!(
            view["state"].as_str(),
            Some("completed" | "failed" | "cancelled")
        ) {
            return (view, submitted, decided);
        }
        if !progressed {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
}

fn assert_weights_close(published: &Weights, reference: &Weights, tol: ToleranceV1) {
    for ((name, p), (_, r)) in published.named().iter().zip(reference.named().iter()) {
        let c = gg_compute::compare::compare_f32(name, p, r, tol);
        assert!(c.passed, "{name}: {:?}", c.checks);
    }
}

#[tokio::test]
async fn collective_run_matches_single_process_reference() {
    let env = start().await;
    let r = setup_researcher(&env, 500).await;
    let (x, y) = dataset();
    let train_x = upload_input(&env, &r, &x).await;
    let train_y = upload_input(&env, &r, &y).await;
    let req = json!({
        "name": "e2e", "train_x": train_x, "train_y": train_y, "d_hidden": 12, "optimizer": "adam",
        "learning_rate": 0.02, "batch_per_round": 96, "tasks_per_round": 3, "rounds": 4, "seed": 5, "max_round_retries": 1,
    });
    // Validation refuses an oversized micro-batch and an unknown optimizer.
    let mut bad = req.clone();
    bad["optimizer"] = json!("rmsprop");
    let (s, e) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/collective", r.project),
            bad,
        )
        .await;
    assert_eq!(s, 422, "{e}");
    let (s, run) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/collective", r.project),
            req,
        )
        .await;
    assert_eq!(s, 200, "{run}");
    assert_eq!(run["state"], "running");
    assert_eq!(run["step"], 0);
    assert_eq!(run["checkpoint_steps"], json!([0]));
    assert_eq!(run["rounds"].as_array().unwrap().len(), 1);
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    // Another researcher's project cannot see the run.
    let other = setup_researcher(&env, 0).await;
    let (s, _) = env
        .get(&other.token, &format!("/researcher/collective/{run_id}"))
        .await;
    assert_eq!(s, 404);

    let workers = vec![
        setup_worker(&env, "w1").await,
        setup_worker(&env, "w2").await,
    ];
    let v = setup_validator(&env).await;
    let (view, submitted, decided) =
        drive(&env, &r, &run_id, &workers, &v, Duration::from_secs(120)).await;
    assert_eq!(view["state"], "completed", "{view}");
    assert_eq!(view["step"], 4);
    assert_eq!(view["checkpoint_steps"], json!([0, 1, 2, 3, 4]));
    let rounds = view["rounds"].as_array().unwrap();
    assert_eq!(rounds.len(), 4);
    assert!(rounds.iter().all(|r| r["state"] == "aggregated"));
    assert_eq!(submitted, 12);
    assert_eq!(decided, 12);

    // Equivalence with a single-process training over the same rounds (ADR-0011 pass criterion).
    let published = download_weights(&env, &r, &view["checkpoint"]["weights"]).await;
    let init = initial_weights(&MlpTask {
        d_in: 6,
        d_h: 12,
        d_out: 2,
        optimizer: Optim::Sgd,
        lr: 0.0,
        batch: 1,
        steps: 1,
        seed: 5,
        n_train: 512,
        n_eval: 0,
    });
    let rounds: Vec<(u64, u64)> = (0..4u64).map(|s| (5 + s, 96)).collect();
    let reference = reference_training(&init, CoordOptim::Adam, 0.02, &x, &y, &rounds).unwrap();
    assert_weights_close(
        &published,
        &reference,
        ToleranceV1 {
            abs: 1e-5,
            rel: 1e-4,
        },
    );
    // ... and differs from the initial weights (training happened).
    let moved = published
        .w1
        .data
        .iter()
        .zip(init.w1.data.iter())
        .any(|(a, b)| (a - b).abs() > 1e-3);
    assert!(
        moved,
        "published weights did not move away from the initial checkpoint"
    );

    // Ledger: 12 accepted tasks paid once, nothing reserved; receipts one per task.
    let (s, budget) = env
        .get(&r.token, &format!("/researcher/projects/{}", r.project))
        .await;
    assert_eq!(s, 200, "{budget}");
    assert_eq!(budget["budget"]["reserved"], "0");
    let (s, receipts) = env
        .get(
            &r.token,
            &format!("/researcher/projects/{}/receipts", r.project),
        )
        .await;
    assert_eq!(s, 200, "{receipts}");
    assert_eq!(receipts.as_array().unwrap().len(), 12);
    env.stop().await;
}

#[tokio::test]
async fn cancelled_round_is_reissued_and_late_results_are_refused() {
    let env = start().await;
    let r = setup_researcher(&env, 500).await;
    let (x, y) = dataset();
    let train_x = upload_input(&env, &r, &x).await;
    let train_y = upload_input(&env, &r, &y).await;
    let req = json!({
        "name": "e2e-faults", "train_x": train_x, "train_y": train_y, "d_hidden": 8, "optimizer": "sgd",
        "learning_rate": 0.05, "batch_per_round": 64, "tasks_per_round": 2, "rounds": 2, "seed": 9, "max_round_retries": 1,
    });
    let (s, run) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/collective", r.project),
            req,
        )
        .await;
    assert_eq!(s, 200, "{run}");
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    let job0 = run["rounds"][0]["job_id"].as_str().unwrap().to_owned();

    let workers = vec![
        setup_worker(&env, "w1").await,
        setup_worker(&env, "w2").await,
    ];
    let v = setup_validator(&env).await;
    // A slow worker computes one task of round 0 but has not submitted yet; the researcher cancels
    // the round's job meanwhile.
    let late = worker_execute(&env, &workers[0])
        .await
        .expect("a round-0 task to claim");
    let (s, c) = env
        .post(
            &r.token,
            &format!("/researcher/jobs/{job0}/cancel"),
            json!({}),
        )
        .await;
    assert_eq!(s, 200, "{c}");
    // The late result is refused: never recorded, never verified, never aggregated.
    let (s, resp) = worker_submit(&env, &workers[0], &late).await;
    assert_eq!(s, 409, "{resp}");

    let (view, _, _) = drive(&env, &r, &run_id, &workers, &v, Duration::from_secs(120)).await;
    assert_eq!(view["state"], "completed", "{view}");
    assert_eq!(view["step"], 2);
    let rounds = view["rounds"].as_array().unwrap();
    let find = |step: i64, attempt: i64| {
        rounds
            .iter()
            .find(|r| r["step"] == step && r["attempt"] == attempt)
            .cloned()
            .unwrap_or(Value::Null)
    };
    assert_eq!(find(0, 1)["state"], "failed");
    assert_eq!(find(0, 1)["job_id"], json!(job0));
    assert_eq!(find(0, 2)["state"], "aggregated");
    assert_eq!(find(1, 1)["state"], "aggregated");
    assert_eq!(view["checkpoint_steps"], json!([0, 1, 2]));
    // The reissued round used the same checkpoint and seed: the result is the reference one.
    let published = download_weights(&env, &r, &view["checkpoint"]["weights"]).await;
    let init = initial_weights(&MlpTask {
        d_in: 6,
        d_h: 8,
        d_out: 2,
        optimizer: Optim::Sgd,
        lr: 0.0,
        batch: 1,
        steps: 1,
        seed: 9,
        n_train: 512,
        n_eval: 0,
    });
    let reference =
        reference_training(&init, CoordOptim::Sgd, 0.05, &x, &y, &[(9, 64), (10, 64)]).unwrap();
    assert_weights_close(
        &published,
        &reference,
        ToleranceV1 {
            abs: 1e-5,
            rel: 1e-4,
        },
    );
    // The cancelled job released its reservation; nothing stays reserved at the end.
    let (s, budget) = env
        .get(&r.token, &format!("/researcher/projects/{}", r.project))
        .await;
    assert_eq!(s, 200, "{budget}");
    assert_eq!(budget["budget"]["reserved"], "0");
    // Cancelling the run itself is idempotent on a completed run and closes nothing.
    let (s, _) = env
        .post(
            &r.token,
            &format!("/researcher/collective/{run_id}/cancel"),
            json!({}),
        )
        .await;
    assert_eq!(s, 204);
    env.stop().await;
}

#[tokio::test]
async fn released_verification_is_claimable_again_and_only_by_its_holder() {
    let env = start().await;
    let r = setup_researcher(&env, 100).await;
    let (x, y) = dataset();
    let train_x = upload_input(&env, &r, &x).await;
    let train_y = upload_input(&env, &r, &y).await;
    let req = json!({
        "name": "release", "train_x": train_x, "train_y": train_y, "d_hidden": 4, "optimizer": "sgd",
        "learning_rate": 0.05, "batch_per_round": 32, "tasks_per_round": 1, "rounds": 1, "seed": 3,
    });
    let (s, run) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/collective", r.project),
            req,
        )
        .await;
    assert_eq!(s, 200, "{run}");
    let w = setup_worker(&env, "w1").await;
    let e = worker_execute(&env, &w).await.expect("one task");
    let (s, resp) = worker_submit(&env, &w, &e).await;
    assert_eq!(s, 200, "{resp}");
    let v1 = setup_validator(&env).await;
    let v2 = setup_validator(&env).await;
    let (s, work) = env
        .post(&v1.token, "/validator/claim", json!({ "wait_secs": 0 }))
        .await;
    assert_eq!(s, 200, "{work}");
    let vid = work["verification_id"].as_str().unwrap().to_owned();
    // Held by v1: nobody else can claim it, and v2 cannot release it.
    let (s, _) = env
        .post(&v2.token, "/validator/claim", json!({ "wait_secs": 0 }))
        .await;
    assert_eq!(s, 204);
    let (s, rel) = env
        .post(
            &v2.token,
            &format!("/validator/verifications/{vid}/release"),
            json!({}),
        )
        .await;
    assert_eq!(s, 200, "{rel}");
    assert_eq!(rel["released"], false);
    // v1 releases it: claimable again immediately (by v2 here), no lease expiry needed.
    let (s, rel) = env
        .post(
            &v1.token,
            &format!("/validator/verifications/{vid}/release"),
            json!({}),
        )
        .await;
    assert_eq!(s, 200, "{rel}");
    assert_eq!(rel["released"], true);
    let (s, work2) = env
        .post(&v2.token, "/validator/claim", json!({ "wait_secs": 0 }))
        .await;
    assert_eq!(s, 200, "{work2}");
    assert_eq!(work2["verification_id"], json!(vid));
    // A second release by v1 (no longer the holder) changes nothing.
    let (s, rel) = env
        .post(
            &v1.token,
            &format!("/validator/verifications/{vid}/release"),
            json!({}),
        )
        .await;
    assert_eq!(s, 200, "{rel}");
    assert_eq!(rel["released"], false);
    let outcome = validator_verify_once(&env, &v2).await;
    assert!(
        outcome.is_none(),
        "v2 already holds the only verification; nothing new to claim"
    );
    env.stop().await;
}

/// Content addressing: two projects that upload identical bytes, or two runs whose checkpoints
/// contain identical tensors (zero biases of the same width), share one artifact row. Each project
/// must still be able to read what it uploaded or what was materialized for it.
#[tokio::test]
async fn identical_artifacts_across_projects_stay_readable_by_each_project() {
    let env = start().await;
    let a = setup_researcher(&env, 100).await;
    let b = setup_researcher(&env, 100).await;
    let (x, y) = dataset();
    let ax = upload_input(&env, &a, &x).await;
    let ay = upload_input(&env, &a, &y).await;
    let bx = upload_input(&env, &b, &x).await;
    let by = upload_input(&env, &b, &y).await;
    assert_eq!(ax.hash, bx.hash);
    assert_eq!(ay.hash, by.hash);
    let req = json!({
        "name": "shared", "train_x": ax, "train_y": ay, "d_hidden": 4, "optimizer": "sgd",
        "learning_rate": 0.05, "batch_per_round": 32, "tasks_per_round": 1, "rounds": 1, "seed": 3,
    });
    let (s, run_a) = env
        .post(
            &a.token,
            &format!("/researcher/projects/{}/collective", a.project),
            req.clone(),
        )
        .await;
    assert_eq!(s, 200, "{run_a}");
    // Project B references the same dataset hashes: it uploaded them, so it may use them.
    let (s, run_b) = env
        .post(
            &b.token,
            &format!("/researcher/projects/{}/collective", b.project),
            req,
        )
        .await;
    assert_eq!(s, 200, "{run_b}");
    // Same width, same protocol seed: identical initial checkpoints, hence shared artifact rows.
    assert_eq!(
        run_a["checkpoint"]["weights"]["b1"]["hash"],
        run_b["checkpoint"]["weights"]["b1"]["hash"]
    );
    let wa = download_weights(&env, &a, &run_a["checkpoint"]["weights"]).await;
    let wb = download_weights(&env, &b, &run_b["checkpoint"]["weights"]).await;
    assert_eq!(wa.b1.data, wb.b1.data);
    // A third project that never uploaded the dataset cannot read it.
    let c = setup_researcher(&env, 0).await;
    let (s, _) = env
        .get_bytes(
            &c.token,
            &format!("/researcher/projects/{}/artifacts/{}", c.project, ax.hash),
        )
        .await;
    assert!(s == 403 || s == 404, "unexpected status {s}");
    env.stop().await;
}

/// A run over a model described by a graph (`graph.grad.v1`): same coordinator, named parameters,
/// checkpoint as one GGB1 bundle; the published checkpoint equals a single-process replay of the
/// same rounds (ADR-0011 pass criterion, generalized by ADR-0016/0017).
#[tokio::test]
async fn graph_run_matches_single_process_reference() {
    graph_run_case(None, Memory::Kept).await;
}

/// The researcher can keep f32 gradients (ADR-0035): the run then matches an unrounded replay.
#[tokio::test]
async fn an_f32_gradient_run_matches_an_unrounded_replay() {
    graph_run_case(Some("f32"), Memory::Kept).await;
}

/// ADR-0036: the optimizer state kept between rounds lost again and again, as restarts would lose
/// it: every step rebuilds the moments from the recorded gradients, and the run ends bit for bit
/// where the single-process replay does.
#[tokio::test]
async fn a_graph_run_survives_losing_its_kept_state() {
    graph_run_case(None, Memory::Forgotten).await;
}

/// ADR-0036: with no memory for the state, every step reads and writes it whole; same result.
#[tokio::test]
async fn a_graph_run_without_kept_state_is_the_same() {
    graph_run_case(None, Memory::Disabled).await;
}

/// ADR-0040: the retention policy running all along, with its state lost again and again, removes
/// nothing the run still needs: bit for bit the replay; then only what is not kept has expired.
#[tokio::test]
async fn retention_removes_only_what_a_graph_run_no_longer_needs() {
    graph_run_case(None, Memory::Pruned).await;
}

/// What happens to the optimizer state the server keeps between rounds (ADR-0036).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Memory {
    /// Kept, as in normal operation.
    Kept,
    /// Dropped every 20 ms, as a restart drops it.
    Forgotten,
    /// None allowed (`step_cache_bytes = 0`).
    Disabled,
    /// Dropped every 20 ms, and the retention policy run every 20 ms with no delay and a
    /// checkpoint kept every 2 steps (ADR-0040): nothing a run still needs may go.
    Pruned,
}

async fn start_memory(memory: Memory) -> Env {
    match memory {
        Memory::Disabled => start_configured(|cfg| cfg.step_cache_bytes = 0).await,
        Memory::Pruned => {
            start_configured(|cfg| {
                cfg.retention_after_secs = 0;
                cfg.checkpoint_keep_every = 2;
            })
            .await
        }
        _ => start().await,
    }
}

/// Drops the kept state every 20 ms while the run goes on, for [`Memory::Forgotten`]; also runs
/// the retention policy for [`Memory::Pruned`].
fn forget_meanwhile(env: &Env, memory: Memory) -> Option<tokio::task::JoinHandle<()>> {
    let state = env.state.clone();
    matches!(memory, Memory::Forgotten | Memory::Pruned).then(|| {
        tokio::spawn(async move {
            loop {
                state.step_cache().clear();
                if memory == Memory::Pruned {
                    gg_control::workflows::retention::prune_once(&state)
                        .await
                        .expect("retention pass");
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    })
}

/// After a run under [`Memory::Pruned`]: one last pass, then the checkpoints kept (step 0, every
/// second step, the last) still download, the others say they expired; workers' intermediate
/// outputs went; nothing a researcher uploaded did (ADR-0040).
async fn check_retention(env: &Env, r: &Researcher, run_id: &str, uploads: &[ArtifactRef]) {
    // The last rounds may have closed after the last pass during the run.
    gg_control::workflows::retention::prune_once(&env.state)
        .await
        .unwrap();
    let run: uuid::Uuid = run_id.parse().unwrap();
    let mut c = env.state.db().pool().acquire().await.unwrap();
    let rows = gg_storage::queries::collective::list_checkpoints(&mut c, run)
        .await
        .unwrap();
    let last = rows.last().unwrap().0;
    let get = |hash: String| {
        let path = format!("/researcher/projects/{}/artifacts/{hash}", r.project);
        async move { env.get_bytes(&r.token, &path).await }
    };
    let weights_of = |ck: &serde_json::Value| {
        let refs: gg_control::workflows::collective::CheckpointRefs =
            serde_json::from_value(ck.clone()).unwrap();
        refs.weights
            .into_values()
            .map(|r| r.hash)
            .collect::<Vec<_>>()
    };
    // A frozen shard keeps one artifact across steps: it stays with the checkpoints kept.
    let kept: Vec<_> = rows
        .iter()
        .filter(|(step, _)| step % 2 == 0 || *step == last)
        .flat_map(|(_, ck)| weights_of(ck))
        .collect();
    let mut expired_steps = 0;
    for (step, ck) in &rows {
        for w in weights_of(ck) {
            let (s, body) = get(w.to_hex()).await;
            // Kept: every second step, the last, and what a checkpoint kept shares (a frozen
            // shard). The run has ended, so the round that produced the last one goes too.
            if step % 2 == 0 || *step == last || kept.contains(&w) {
                assert_eq!(s, 200, "kept checkpoint of step {step}");
            } else {
                assert_eq!(s, 404, "step {step} should have expired");
                assert!(
                    String::from_utf8_lossy(&body).contains("expired"),
                    "step {step}"
                );
                expired_steps += 1;
            }
        }
    }
    assert!(expired_steps > 0, "no intermediate checkpoint expired");
    for u in uploads {
        assert_eq!(get(u.hash.to_hex()).await.0, 200, "a researcher's upload");
    }
    let count = |who: &'static str| {
        let q = format!(
            "SELECT count(*) FROM artifacts WHERE expired_at IS NOT NULL AND uploaded_by LIKE '{who}:%'"
        );
        let pool = env.state.db().pool().clone();
        async move {
            sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(q))
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert!(count("worker").await > 0, "workers' intermediate outputs");
    assert_eq!(count("researcher").await, 0);
}

/// What the run's steps did with the kept state, checked against `memory`.
fn check_memory(env: &Env, memory: Memory, forgetting: Option<tokio::task::JoinHandle<()>>) {
    if let Some(h) = forgetting {
        h.abort();
    }
    let rebuilt = env.state.step_cache().rebuilt();
    match memory {
        Memory::Kept | Memory::Disabled => assert_eq!(rebuilt, 0, "{memory:?}"),
        Memory::Forgotten | Memory::Pruned => {
            assert!(rebuilt > 0, "no step had to rebuild its moments")
        }
    }
    // Nothing is left in memory once the run has completed.
    assert_eq!(env.state.step_cache().used_bytes(), 0);
}

async fn graph_run_case(precision: Option<&str>, memory: Memory) {
    use gg_compute::grad::CoordOptim;
    use gg_compute::graph::exec::{self, Prepared};
    use gg_compute::graph::task::{
        NamedOptimizerState, aggregate_named, apply_step_named, dropout_seed, round_indices,
        select_rows,
    };
    use gg_protocol::messages::GraphGradParamsV1;
    use gg_protocol::tensor_file::BundleF32;

    const GRAPH: &str = r#"{
      "format": "gg-graph/v1", "name": "e2e-graph",
      "inputs": [{"name": "x", "shape": [0, 6]}],
      "parameters": [
        {"name": "w1", "shape": [6, 10], "init": "xavier_uniform"},
        {"name": "b1", "shape": [10], "init": "zeros"},
        {"name": "w2", "shape": [10, 3], "init": "xavier_uniform"},
        {"name": "b2", "shape": [3], "init": "zeros"}
      ],
      "nodes": [
        {"op": "MatMul", "inputs": ["x", "w1"], "output": "h"},
        {"op": "Add", "inputs": ["h", "b1"], "output": "hb"},
        {"op": "Gelu", "inputs": ["hb"], "output": "a"},
        {"op": "MatMul", "inputs": ["a", "w2"], "output": "o"},
        {"op": "Add", "inputs": ["o", "b2"], "output": "logits"}
      ],
      "loss": {"cross_entropy": {"prediction": "logits", "target": {"name": "y", "shape": [0], "dtype": "i64"}}}
    }"#;
    let env = start_memory(memory).await;
    let r = setup_researcher(&env, 500).await;
    let total = 96u64;
    let mut d = gg_protocol::drbg::Drbg::new("e2e/graph", 1);
    let x = TensorF32::new(
        vec![total, 6],
        (0..total * 6).map(|_| d.next_sym_f32()).collect(),
    )
    .unwrap();
    let y = TensorF32::new(
        vec![total],
        (0..total).map(|_| d.next_index(3) as f32).collect(),
    )
    .unwrap();
    let graph_ref = upload_bytes(&env, &r, GRAPH.as_bytes().to_vec(), "gg-graph-v1").await;
    let x_ref = upload_input(&env, &r, &x).await;
    let y_ref = upload_input(&env, &r, &y).await;
    let req = json!({
        "name": "e2e-graph", "graph": graph_ref, "datasets": {"x": x_ref, "y": y_ref},
        "optimizer": "adam", "learning_rate": 0.02, "batch_per_round": 48, "tasks_per_round": 3,
        "rounds": 3, "seed": 5, "max_round_retries": 1,
    });
    let mut req = req;
    if let Some(p) = precision {
        req["gradient_precision"] = json!(p);
    }
    let bf16 = precision != Some("f32");
    // An unknown gradient precision is refused.
    let mut bad = req.clone();
    bad["gradient_precision"] = json!("fp8");
    let (s, e) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/collective", r.project),
            bad,
        )
        .await;
    assert_eq!(s, 422, "{e}");
    assert!(e.to_string().contains("gradient_precision"), "{e}");
    // A dataset missing from the request is refused with its name.
    let mut bad = req.clone();
    bad["datasets"] = json!({"x": x_ref});
    let (s, e) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/collective", r.project),
            bad,
        )
        .await;
    assert_eq!(s, 422, "{e}");
    assert!(e.to_string().contains("dataset"), "{e}");
    let (s, run) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/collective", r.project),
            req,
        )
        .await;
    assert_eq!(s, 200, "{run}");
    assert_eq!(run["state"], "running");
    assert_eq!(
        run["config"]["graph"]["parameters"],
        6 * 10 + 10 + 10 * 3 + 3
    );
    assert_eq!(run["config"]["graph"]["gradient_bf16"], bf16);
    // 4 tensors: 8 bytes of bundle header, 4 × (index entry + GGT1 header of rank 1 or 2).
    let headers = 8 + (2 + 2 + 8) * 4 + (6 + 16) * 2 + (6 + 8) * 2;
    assert_eq!(
        run["config"]["graph"]["gradient_bf16_bytes"],
        headers + 2 * (6 * 10 + 10 + 10 * 3 + 3)
    );
    assert_eq!(
        run["config"]["graph"]["gradient_bytes"],
        headers + 4 * (6 * 10 + 10 + 10 * 3 + 3)
    );
    let run_id = run["run_id"].as_str().unwrap().to_owned();

    let workers = vec![
        setup_worker(&env, "g1").await,
        setup_worker(&env, "g2").await,
    ];
    let v = setup_validator(&env).await;
    let forgetting = forget_meanwhile(&env, memory);
    let (view, submitted, decided) =
        drive(&env, &r, &run_id, &workers, &v, Duration::from_secs(120)).await;
    check_memory(&env, memory, forgetting);
    assert_eq!(view["state"], "completed", "{view}");
    assert_eq!(view["step"], 3);
    assert_eq!(submitted, 9);
    assert_eq!(decided, 9);

    // Published checkpoint bundle.
    let ck: ArtifactRef =
        serde_json::from_value(view["checkpoint"]["weights"]["checkpoint"].clone()).unwrap();
    let (s, bytes) = env
        .get_bytes(
            &r.token,
            &format!("/researcher/projects/{}/artifacts/{}", r.project, ck.hash),
        )
        .await;
    assert_eq!(s, 200);
    let published = BundleF32::decode(&bytes, &Default::default())
        .unwrap()
        .entries;

    // Single-process replay of the same rounds from the same seeded initialization.
    let graph: gg_compute::graph::ModelGraphV1 = serde_json::from_str(GRAPH).unwrap();
    let prepared = Prepared::new(&graph).unwrap();
    let mut weights = exec::init_parameters(&graph, 5);
    let mut state = NamedOptimizerState::default();
    for step in 0..3u64 {
        let seed = 5 + step;
        let mut parts = Vec::new();
        for (start, count) in gg_control::workflows::collective::slices(48, 3) {
            let idx = round_indices(seed, total, start, count);
            let batch = BTreeMap::from([
                ("x".to_string(), select_rows(&x, &idx)),
                ("y".to_string(), select_rows(&y, &idx)),
            ]);
            let p = GraphGradParamsV1 {
                graph_hash: graph_ref.hash.to_hex(),
                graph_bytes: graph_ref.size,
                parameters: 103,
                checkpoint_bytes: 0,
                gradient_bytes: 0,
                segments: 1,
                offload_parameters: false,
                training_flops_per_example: 0,
                activation_bytes_per_example: 0,
                datasets: vec![],
                round_seed: seed,
                example_start: start,
                example_count: count,
                total_examples: total,
                shard_bytes: 0,
                batch_inputs: false,
                gradient_bf16: false,
            };
            let step_result =
                exec::gradient_cpu(&prepared, &weights, &batch, dropout_seed(&p)).unwrap();
            parts.push((
                count,
                if bf16 {
                    as_sent(step_result.gradients)
                } else {
                    step_result.gradients
                },
            ));
        }
        let g = aggregate_named(&parts).unwrap();
        apply_step_named(CoordOptim::Adam, 0.02, &mut weights, &g, &mut state).unwrap();
    }
    assert_eq!(published.len(), weights.len());
    for (name, t) in &weights {
        let got = &published[name];
        assert_eq!(got.shape, t.shape, "{name}");
        let max = got
            .data
            .iter()
            .zip(&t.data)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            max <= 1e-6,
            "{name}: deviation {max} between the published checkpoint and the reference"
        );
    }
    if memory == Memory::Pruned {
        check_retention(&env, &r, &run_id, &[graph_ref, x_ref, y_ref]).await;
    }
}

/// The three memory levers of ADR-0019 survive a real distributed run: frozen parameters,
/// segmented activation recomputation, parameter offloading. What must hold: frozen parameters do
/// not move, trained ones equal what a single-process replay gives, and the exchanged gradient
/// carries only the trained ones.
#[tokio::test]
async fn memory_levers_survive_a_distributed_run() {
    use gg_compute::grad::CoordOptim;
    use gg_compute::graph::exec::{self, ExecOptions, Prepared};
    use gg_compute::graph::task::{
        NamedOptimizerState, aggregate_named, apply_step_named, dropout_seed, gradient_bundle_size,
        round_indices, select_rows,
    };
    use gg_protocol::messages::GraphGradParamsV1;
    use gg_protocol::tensor_file::BundleF32;

    // A frozen base (w1, b1) and a trained head (w2, b2): the shape of adapter fine-tuning.
    const GRAPH: &str = r#"{
      "format": "gg-graph/v1", "name": "frozen",
      "inputs": [{"name": "x", "shape": [0, 6]}],
      "parameters": [
        {"name": "w1", "shape": [6, 12], "init": "xavier_uniform", "trainable": false},
        {"name": "b1", "shape": [12], "init": "zeros", "trainable": false},
        {"name": "w2", "shape": [12, 3], "init": "xavier_uniform"},
        {"name": "b2", "shape": [3], "init": "zeros"}
      ],
      "nodes": [
        {"op": "MatMul", "inputs": ["x", "w1"], "output": "h"},
        {"op": "Add", "inputs": ["h", "b1"], "output": "hb"},
        {"op": "Gelu", "inputs": ["hb"], "output": "a"},
        {"op": "MatMul", "inputs": ["a", "w2"], "output": "o"},
        {"op": "Add", "inputs": ["o", "b2"], "output": "logits"}
      ],
      "loss": {"cross_entropy": {"prediction": "logits", "target": {"name": "y", "shape": [0], "dtype": "i64"}}}
    }"#;
    let env = start().await;
    let r = setup_researcher(&env, 500).await;
    let total = 96u64;
    let mut d = gg_protocol::drbg::Drbg::new("e2e/frozen", 1);
    let x = TensorF32::new(
        vec![total, 6],
        (0..total * 6).map(|_| d.next_sym_f32()).collect(),
    )
    .unwrap();
    let y = TensorF32::new(
        vec![total],
        (0..total).map(|_| d.next_index(3) as f32).collect(),
    )
    .unwrap();
    let graph_ref = upload_bytes(&env, &r, GRAPH.as_bytes().to_vec(), "gg-graph-v1").await;
    let x_ref = upload_input(&env, &r, &x).await;
    let y_ref = upload_input(&env, &r, &y).await;
    let (s, run) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/collective", r.project),
            json!({
                "name": "frozen", "graph": graph_ref, "datasets": {"x": x_ref, "y": y_ref},
                "optimizer": "adam", "learning_rate": 0.05, "batch_per_round": 48,
                "tasks_per_round": 3, "rounds": 3, "seed": 5,
                "segments": 4, "offload_parameters": true,
            }),
        )
        .await;
    assert_eq!(s, 200, "{run}");
    let graph: gg_compute::graph::ModelGraphV1 = serde_json::from_str(GRAPH).unwrap();
    // The exchanged gradient carries only the head: 12×3 + 3 = 39 values, against 123 parameters.
    assert_eq!(run["config"]["graph"]["parameters"], 123);
    assert_eq!(
        run["config"]["graph"]["gradient_bytes"].as_u64().unwrap(),
        gradient_bundle_size(&graph)
    );
    assert!(
        run["config"]["graph"]["gradient_bytes"].as_u64().unwrap()
            < run["config"]["graph"]["checkpoint_bytes"].as_u64().unwrap() / 2,
        "freezing must clearly reduce what is exchanged: {}",
        run["config"]["graph"]
    );
    assert_eq!(run["config"]["graph"]["segments"], 4);
    assert_eq!(run["config"]["graph"]["offload_parameters"], true);
    let run_id = run["run_id"].as_str().unwrap().to_owned();

    let workers = vec![
        setup_worker(&env, "f1").await,
        setup_worker(&env, "f2").await,
    ];
    let v = setup_validator(&env).await;
    let (view, submitted, decided) =
        drive(&env, &r, &run_id, &workers, &v, Duration::from_secs(120)).await;
    assert_eq!(view["state"], "completed", "{view}");
    assert_eq!((submitted, decided), (9, 9));

    let ck: ArtifactRef =
        serde_json::from_value(view["checkpoint"]["weights"]["checkpoint"].clone()).unwrap();
    let (s, bytes) = env
        .get_bytes(
            &r.token,
            &format!("/researcher/projects/{}/artifacts/{}", r.project, ck.hash),
        )
        .await;
    assert_eq!(s, 200);
    let published = BundleF32::decode(&bytes, &Default::default())
        .unwrap()
        .entries;

    // Single-process replay, with the same levers.
    let prepared = Prepared::new(&graph).unwrap();
    let initial = exec::init_parameters(&graph, 5);
    let mut weights = initial.clone();
    let mut state = NamedOptimizerState::default();
    let options = ExecOptions {
        segments: 4,
        offload_parameters: true,
    };
    for step in 0..3u64 {
        let seed = 5 + step;
        let mut parts = Vec::new();
        for (start, count) in gg_control::workflows::collective::slices(48, 3) {
            let idx = round_indices(seed, total, start, count);
            let batch = BTreeMap::from([
                ("x".to_string(), select_rows(&x, &idx)),
                ("y".to_string(), select_rows(&y, &idx)),
            ]);
            let p = GraphGradParamsV1 {
                graph_hash: graph_ref.hash.to_hex(),
                graph_bytes: graph_ref.size,
                parameters: 123,
                checkpoint_bytes: 0,
                gradient_bytes: 0,
                segments: 4,
                offload_parameters: true,
                training_flops_per_example: 0,
                activation_bytes_per_example: 0,
                datasets: vec![],
                round_seed: seed,
                example_start: start,
                example_count: count,
                total_examples: total,
                shard_bytes: 0,
                batch_inputs: false,
                gradient_bf16: false,
            };
            let step_result =
                exec::gradient_cpu_with(&prepared, &weights, &batch, dropout_seed(&p), options)
                    .unwrap();
            assert_eq!(step_result.gradients.len(), 2, "only w2 and b2 are trained");
            parts.push((count, as_sent(step_result.gradients)));
        }
        let g = aggregate_named(&parts).unwrap();
        apply_step_named(CoordOptim::Adam, 0.05, &mut weights, &g, &mut state).unwrap();
    }

    // The published checkpoint carries all 4 parameters, frozen ones included.
    assert_eq!(published.len(), 4);
    for (name, t) in &weights {
        let got = &published[name];
        let max = got
            .data
            .iter()
            .zip(&t.data)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(max <= 1e-6, "{name}: deviation {max}");
    }
    // Frozen parameters stayed exactly at their initial value; trained ones moved.
    for name in ["w1", "b1"] {
        assert_eq!(
            published[name].encode(),
            initial[name].encode(),
            "{name} moved"
        );
    }
    for name in ["w2", "b2"] {
        assert_ne!(
            published[name].encode(),
            initial[name].encode(),
            "{name} did not move"
        );
    }
}

/// Claim with announced capabilities; return the status and the response.
async fn claim_with(env: &Env, w: &WorkerCtx, caps: Value) -> (u16, Value) {
    let mut body = json!({ "kinds": ["graph.grad.v1"], "wait_secs": 0 });
    for (k, v) in caps.as_object().unwrap() {
        body[k] = v.clone();
    }
    env.post(&w.token, "/worker/claim", body).await
}

/// C1 (docs/ROADMAP.md): a task only goes to a worker that announces enough memory and a recent
/// enough profile; the executor itself refuses a task larger than the declared memory; the
/// announced capacity is readable by a researcher before submitting.
#[tokio::test]
async fn a_task_only_goes_to_a_worker_that_can_hold_it() {
    use gg_compute::graph::templates::{Positions, TransformerDims, transformer};

    let env = start().await;
    let r = setup_researcher(&env, 500).await;
    let (seq, vocab, total) = (8u64, 30u64, 64u64);
    let model = transformer(TransformerDims {
        layers: 2,
        dim: 32,
        heads: 4,
        seq,
        vocab,
        ffn: 64,
        causal: true,
        positions: Positions::rotary(10000.0),
    })
    .unwrap();
    let mut d = gg_protocol::drbg::Drbg::new("e2e/caps", 1);
    let ids = |d: &mut gg_protocol::drbg::Drbg| {
        TensorF32::new(
            vec![total, seq],
            (0..total * seq)
                .map(|_| d.next_index(vocab as u32) as f32)
                .collect(),
        )
        .unwrap()
    };
    let (x, y) = (ids(&mut d), ids(&mut d));
    let graph_ref =
        upload_bytes(&env, &r, serde_json::to_vec(&model).unwrap(), "gg-graph-v1").await;
    let x_ref = upload_input(&env, &r, &x).await;
    let y_ref = upload_input(&env, &r, &y).await;
    let (s, run) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/collective", r.project),
            json!({
                "name": "caps", "graph": graph_ref, "datasets": {"ids": x_ref, "y": y_ref},
                "optimizer": "adam", "learning_rate": 0.01, "batch_per_round": 16,
                "tasks_per_round": 2, "rounds": 1, "seed": 3, "max_round_retries": 1,
            }),
        )
        .await;
    assert_eq!(s, 200, "{run}");
    let need = run["config"]["graph"]["task_device_bytes"]
        .as_u64()
        .unwrap();
    assert_eq!(
        run["config"]["graph"]["required_profile"], 3,
        "RoPE: gg-ops/3"
    );
    assert!(need > 0);

    let small = setup_worker(&env, "small").await;
    let old = setup_worker(&env, "old").await;
    let fit = setup_worker(&env, "fit").await;
    // Too little memory: nothing is offered.
    let (s, c) = claim_with(
        &env,
        &small,
        json!({"device_memory_bytes": need - 1, "ram_bytes": 64u64 << 30, "profile": "gg-ops/4"}),
    )
    .await;
    assert_eq!(s, 204, "{c}");
    // Enough memory, profile too old: nothing either.
    let (s, c) = claim_with(
        &env,
        &old,
        json!({"device_memory_bytes": need, "ram_bytes": 64u64 << 30, "profile": "gg-ops/2"}),
    )
    .await;
    assert_eq!(s, 204, "{c}");
    // A pre-C1 worker, which announces nothing, is treated as gg-ops/1.
    let (s, c) = claim_with(&env, &old, json!({})).await;
    assert_eq!(s, 204, "{c}");
    // Unreadable announcement: refused.
    let (s, _) = claim_with(&env, &old, json!({"profile": "onnx/17"})).await;
    assert_eq!(s, 422);
    // Just enough: the task goes out.
    let (s, claim) = claim_with(
        &env,
        &fit,
        json!({"device_memory_bytes": need, "ram_bytes": 64u64 << 30, "profile": "gg-ops/4", "engine": "cpu"}),
    )
    .await;
    assert_eq!(s, 200, "{claim}");

    // The executor itself enforces the declared limit, before any computation.
    let spec: TaskSpecV1 = serde_json::from_value(claim["spec"].clone()).unwrap();
    let task = gg_compute::validate_definition(&spec.definition).unwrap();
    let mut inputs = BTreeMap::new();
    for inp in claim["inputs"].as_array().unwrap() {
        let name = inp["name"].as_str().unwrap();
        let hash = inp["hash"].as_str().unwrap();
        let (s, bytes) = env
            .get_bytes(&fit.token, &format!("/worker/artifacts/{hash}"))
            .await;
        assert_eq!(s, 200);
        let p = fit.dir.path().join(format!("in-{name}"));
        std::fs::write(&p, bytes).unwrap();
        inputs.insert(name.to_owned(), p);
    }
    let out = fit.dir.path().join("out");
    let refused = gg_compute::execute_task_limited(
        EngineSelection::FlexCpu,
        &task,
        &inputs,
        &out,
        Some(need / 2),
    )
    .expect_err("task larger than the declared memory");
    assert!(refused.to_string().contains("memory limit"), "{refused}");
    gg_compute::execute_task_limited(EngineSelection::FlexCpu, &task, &inputs, &out, Some(need))
        .expect("fits in the announced memory");

    // What the grid announces, readable before submitting.
    let (s, cap) = env.get(&r.token, "/researcher/grid/capacity").await;
    assert_eq!(s, 200, "{cap}");
    let workers = cap["workers"].as_array().unwrap();
    assert_eq!(workers.len(), 3, "{cap}");
    assert_eq!(workers[0]["device_memory_bytes"], need);
    assert_eq!(cap["server_profile"], gg_compute::graph::PROFILE_VERSION);
    env.stop().await;
}

/// The published checkpoint of a graph run, reassembled from its parts.
async fn merged_checkpoint(env: &Env, r: &Researcher, view: &Value) -> BTreeMap<String, TensorF32> {
    use gg_protocol::tensor_file::{BundleF32, TensorLimits};
    let mut out = BTreeMap::new();
    for (name, value) in view["checkpoint"]["weights"].as_object().unwrap() {
        assert!(name.starts_with("checkpoint"), "{name}");
        let a: ArtifactRef = serde_json::from_value(value.clone()).unwrap();
        let (s, bytes) = env
            .get_bytes(
                &r.token,
                &format!("/researcher/projects/{}/artifacts/{}", r.project, a.hash),
            )
            .await;
        assert_eq!(s, 200);
        out.extend(
            BundleF32::decode(
                &bytes,
                &TensorLimits {
                    max_elements: 1 << 24,
                },
            )
            .unwrap()
            .entries,
        );
    }
    out
}

/// C2 (docs/ROADMAP.md): the same run, as a single piece or in shards, publishes **exactly** the
/// same weights; so do initial weights uploaded parameter by parameter. A frozen shard is never
/// rewritten, and optimizer state exists only for trained shards.
#[tokio::test]
async fn a_sharded_checkpoint_gives_exactly_the_same_training() {
    use gg_compute::graph::templates::{Positions, TransformerDims, transformer};

    let env = start().await;
    let r = setup_researcher(&env, 2000).await;
    let (seq, vocab, total) = (8u64, 30u64, 64u64);
    let model = transformer(TransformerDims {
        layers: 2,
        dim: 32,
        heads: 4,
        seq,
        vocab,
        ffn: 64,
        causal: true,
        positions: Positions::rotary(10000.0),
    })
    .unwrap();
    let mut d = gg_protocol::drbg::Drbg::new("e2e/shards", 1);
    let ids = |d: &mut gg_protocol::drbg::Drbg| {
        TensorF32::new(
            vec![total, seq],
            (0..total * seq)
                .map(|_| d.next_index(vocab as u32) as f32)
                .collect(),
        )
        .unwrap()
    };
    let (x, y) = (ids(&mut d), ids(&mut d));
    let graph_ref =
        upload_bytes(&env, &r, serde_json::to_vec(&model).unwrap(), "gg-graph-v1").await;
    let x_ref = upload_input(&env, &r, &x).await;
    let y_ref = upload_input(&env, &r, &y).await;
    let seed = 9u64;
    let mut initial = BTreeMap::new();
    for (name, t) in gg_compute::graph::exec::init_parameters(&model, seed) {
        initial.insert(name, upload_input(&env, &r, &t).await);
    }
    let request = |shard_bytes: u64, per_parameter: bool| {
        let mut req = json!({
            "name": format!("shards-{shard_bytes}-{per_parameter}"), "graph": graph_ref,
            "datasets": {"ids": x_ref, "y": y_ref}, "optimizer": "adam", "learning_rate": 0.01,
            "batch_per_round": 16, "tasks_per_round": 2, "rounds": 3, "seed": seed,
            "max_round_retries": 1, "shard_bytes": shard_bytes,
        });
        if per_parameter {
            req["initial_parameters"] = serde_json::to_value(&initial).unwrap();
        }
        req
    };
    let workers = vec![
        setup_worker(&env, "t1").await,
        setup_worker(&env, "t2").await,
    ];
    let v = setup_validator(&env).await;
    let mut views = Vec::new();
    for (shard_bytes, per_parameter) in [(0u64, false), (3_000, false), (3_000, true)] {
        let (s, run) = env
            .post(
                &r.token,
                &format!("/researcher/projects/{}/collective", r.project),
                request(shard_bytes, per_parameter),
            )
            .await;
        assert_eq!(s, 200, "{run}");
        let initial_parts = run["checkpoint"]["weights"].clone();
        let run_id = run["run_id"].as_str().unwrap().to_owned();
        let (view, _, _) = drive(&env, &r, &run_id, &workers, &v, Duration::from_secs(180)).await;
        assert_eq!(view["state"], "completed", "{view}");
        assert_eq!(view["step"], 3);
        views.push((shard_bytes, initial_parts, view));
    }
    let whole = merged_checkpoint(&env, &r, &views[0].2).await;
    for (shard_bytes, initial_parts, view) in &views[1..] {
        let parts = view["checkpoint"]["weights"].as_object().unwrap();
        assert!(parts.len() >= 4, "{shard_bytes}: {parts:?}");
        assert!(parts.keys().all(|k| k.starts_with("checkpoint.")));
        let got = merged_checkpoint(&env, &r, view).await;
        assert_eq!(got.len(), whole.len());
        for (name, t) in &whole {
            assert_eq!(
                &got[name], t,
                "{name} differs between a single piece and shards"
            );
        }
        // Frozen shards (RoPE tables, constant) keep their original artifact.
        let m = view["checkpoint"]["opt_m"].as_object().unwrap();
        let frozen: Vec<&String> = parts.keys().filter(|k| !m.contains_key(*k)).collect();
        assert!(!frozen.is_empty(), "no frozen shard?");
        for k in frozen {
            assert_eq!(parts[k], initial_parts[k], "frozen shard {k} rewritten");
        }
    }
    env.stop().await;
}

/// C4 (docs/ROADMAP.md): a dataset uploaded in row shards, with tasks that receive only their
/// micro-batch, gives **exactly** the same training as a single-piece dataset downloaded in full
/// by every task. And the plan, requested before submitting, picks the levers from the memory the
/// workers announce.
#[tokio::test]
async fn server_provided_micro_batches_give_the_same_training() {
    use gg_compute::graph::templates::{Positions, TransformerDims, transformer};

    let env = start().await;
    let r = setup_researcher(&env, 2000).await;
    let (seq, vocab, total) = (8u64, 30u64, 96u64);
    let model = transformer(TransformerDims {
        layers: 2,
        dim: 32,
        heads: 4,
        seq,
        vocab,
        ffn: 64,
        causal: true,
        positions: Positions::rotary(10000.0),
    })
    .unwrap();
    let mut d = gg_protocol::drbg::Drbg::new("e2e/batch-inputs", 1);
    let ids = |d: &mut gg_protocol::drbg::Drbg| {
        TensorF32::new(
            vec![total, seq],
            (0..total * seq)
                .map(|_| d.next_index(vocab as u32) as f32)
                .collect(),
        )
        .unwrap()
    };
    let (x, y) = (ids(&mut d), ids(&mut d));
    let rows = |t: &TensorF32, a: u64, b: u64| {
        TensorF32::new(
            vec![b - a, seq],
            t.data[(a * seq) as usize..(b * seq) as usize].to_vec(),
        )
        .unwrap()
    };
    let graph_ref =
        upload_bytes(&env, &r, serde_json::to_vec(&model).unwrap(), "gg-graph-v1").await;
    let x_ref = upload_input(&env, &r, &x).await;
    let y_ref = upload_input(&env, &r, &y).await;
    let mut x_parts = Vec::new();
    for (a, b) in [(0u64, 40u64), (40, 41), (41, 96)] {
        x_parts.push(upload_input(&env, &r, &rows(&x, a, b)).await);
    }

    // The plan, before any submission: without workers it says so; with them it chooses.
    let plan_req = json!({"graph": model, "batch_per_round": 16, "tasks_per_round": 2});
    let (s, plan) = env
        .post(&r.token, "/researcher/plan", plan_req.clone())
        .await;
    assert_eq!(s, 200, "{plan}");
    assert_eq!(plan["required_profile"], "gg-ops/3");
    assert_eq!(plan["workers_seen"], 0);
    assert!(plan["recommendation"].is_null());
    let plain = plan["plain_task_device_bytes"].as_u64().unwrap();
    let task_ram = plan["task_ram_bytes"].as_u64().unwrap();

    let workers = vec![
        setup_worker(&env, "b1").await,
        setup_worker(&env, "b2").await,
    ];
    // A large accelerator does not make up for too little host memory: the scheduler would never
    // give this worker the task, so the plan does not count it, and says why.
    let lowram = setup_worker(&env, "lowram").await;
    let (s, _) = claim_with(
        &env,
        &lowram,
        json!({"device_memory_bytes": 64u64 << 30, "ram_bytes": task_ram - 1, "profile": "gg-ops/6"}),
    )
    .await;
    assert_eq!(s, 204);
    let (_, plan) = env
        .post(&r.token, "/researcher/plan", plan_req.clone())
        .await;
    assert_eq!(plan["workers_with_profile"], 1, "{plan}");
    assert_eq!(plan["workers_with_ram"], 0, "{plan}");
    assert!(plan["recommendation"].is_null(), "{plan}");
    assert!(
        plan["advice"].as_str().unwrap().contains("host memory"),
        "{plan}"
    );
    // A worker too small for the task without levers, but large enough with them: the plan picks
    // segments; the worker announces itself when claiming (nothing to do: no task yet).
    let small = setup_worker(&env, "small").await;
    let (s, _) = claim_with(
        &env,
        &small,
        json!({"device_memory_bytes": plain - 1, "ram_bytes": 64u64 << 30, "profile": "gg-ops/6"}),
    )
    .await;
    assert_eq!(s, 204);
    let (_, plan) = env
        .post(&r.token, "/researcher/plan", plan_req.clone())
        .await;
    let rec = &plan["recommendation"];
    assert!(rec["segments"].as_u64().unwrap() > 1, "{plan}");
    assert!(rec["task_device_bytes"].as_u64().unwrap() < plain);
    assert_eq!(rec["fitting_workers"], 1, "{plan}");
    // A huge micro-batch: nothing fits, the plan says what to do.
    let (_, plan_big) = env
        .post(
            &r.token,
            "/researcher/plan",
            json!({"graph": model, "batch_per_round": 4096, "tasks_per_round": 1}),
        )
        .await;
    assert!(plan_big["recommendation"].is_null(), "{plan_big}");
    assert!(
        plan_big["advice"]
            .as_str()
            .unwrap()
            .contains("tasks_per_round"),
        "{plan_big}"
    );

    let v = setup_validator(&env).await;
    let base = json!({
        "graph": graph_ref, "optimizer": "adam", "learning_rate": 0.01, "batch_per_round": 16,
        "tasks_per_round": 2, "rounds": 3, "seed": 4, "max_round_retries": 1,
    });
    let mut whole = base.clone();
    whole["name"] = json!("whole");
    whole["datasets"] = json!({"ids": x_ref, "y": y_ref});
    whole["batch_inputs"] = json!(false);
    let mut parts = base.clone();
    parts["name"] = json!("shards");
    parts["datasets"] = json!({"y": y_ref});
    parts["dataset_parts"] = json!({"ids": x_parts});
    // Shards require server-provided micro-batches.
    let mut bad = parts.clone();
    bad["batch_inputs"] = json!(false);
    let (s, e) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/collective", r.project),
            bad,
        )
        .await;
    assert_eq!(s, 422, "{e}");

    let mut views = Vec::new();
    for req in [whole, parts] {
        let (s, run) = env
            .post(
                &r.token,
                &format!("/researcher/projects/{}/collective", r.project),
                req,
            )
            .await;
        assert_eq!(s, 200, "{run}");
        let run_id = run["run_id"].as_str().unwrap().to_owned();
        let (view, _, _) = drive(&env, &r, &run_id, &workers, &v, Duration::from_secs(180)).await;
        assert_eq!(view["state"], "completed", "{view}");
        views.push(view);
    }
    assert_eq!(views[1]["config"]["graph"]["batch_inputs"], true);
    let a = merged_checkpoint(&env, &r, &views[0]).await;
    let b = merged_checkpoint(&env, &r, &views[1]).await;
    for (name, t) in &a {
        assert_eq!(
            &b[name], t,
            "{name}: provided micro-batches and whole dataset differ"
        );
    }
    // The tasks of the second run received only their micro-batch: 8 rows, not 96.
    let mut c = env.tdb.as_ref().unwrap().db.pool().acquire().await.unwrap();
    let specs: Vec<(Vec<u8>,)> = sqlx::query_as("SELECT spec_cbor FROM tasks")
        .fetch_all(&mut *c)
        .await
        .unwrap();
    drop(c);
    let mut micro = 0;
    for (cbor,) in specs {
        let spec: TaskSpecV1 = gg_protocol::canonical::from_bytes(&cbor).unwrap();
        if let TaskParamsV1::GraphGrad(p) = &spec.definition.params {
            let size = spec.definition.inputs["ids"].size;
            if p.batch_inputs {
                assert_eq!(
                    size,
                    gg_compute::catalog::expected_rgt_size(&[p.example_count as u64, seq])
                );
                micro += 1;
            } else {
                assert_eq!(size, gg_compute::catalog::expected_rgt_size(&[total, seq]));
            }
        }
    }
    assert_eq!(
        micro, 6,
        "three rounds of two tasks with a provided micro-batch"
    );
    env.stop().await;
}

/// D2 (docs/ROADMAP.md): a model larger than every worker trains in stages. Each worker announces
/// less memory than the whole model requires; the server partitions the model under that memory,
/// orders the waves (forward passes, then backward), has every stage verified and aggregates shard
/// by shard. The published checkpoint equals **exactly** a single-process replay of the same
/// stages, and the same as a single-machine training up to rounding.
#[tokio::test]
async fn a_model_larger_than_every_worker_trains_in_stages() {
    staged_case(Memory::Kept).await;
}

/// ADR-0036 for a staged run: the moments rebuilt from a staged round's partial gradients give the
/// same training bit for bit.
#[tokio::test]
async fn a_staged_run_survives_losing_its_kept_state() {
    staged_case(Memory::Forgotten).await;
}

/// ADR-0040 for a staged run: the retention policy running all along keeps what the stages, the
/// replay of the moments and the run still need; bit for bit the replay.
#[tokio::test]
async fn retention_removes_only_what_a_staged_run_no_longer_needs() {
    staged_case(Memory::Pruned).await;
}

async fn staged_case(memory: Memory) {
    use gg_compute::grad::CoordOptim;
    use gg_compute::graph::exec::{self, Prepared};
    use gg_compute::graph::task::{
        NamedOptimizerState, aggregate_named, apply_step_named, round_indices, select_rows,
    };
    use gg_compute::graph::templates::{Positions, TransformerDims, transformer};
    use gg_compute::graph::{memory, stages};

    let env = start_memory(memory).await;
    let r = setup_researcher(&env, 50_000).await;
    let (seq, vocab, total) = (8u64, 30u64, 64u64);
    let model = transformer(TransformerDims {
        layers: 3,
        dim: 32,
        heads: 4,
        seq,
        vocab,
        ffn: 64,
        causal: true,
        positions: Positions::rotary(10000.0),
    })
    .unwrap();
    let report = gg_compute::graph::validate(&model).unwrap();
    let mut d = gg_protocol::drbg::Drbg::new("e2e/stages", 1);
    let ids = |d: &mut gg_protocol::drbg::Drbg| {
        TensorF32::new(
            vec![total, seq],
            (0..total * seq)
                .map(|_| d.next_index(vocab as u32) as f32)
                .collect(),
        )
        .unwrap()
    };
    let (x, y) = (ids(&mut d), ids(&mut d));
    let graph_ref =
        upload_bytes(&env, &r, serde_json::to_vec(&model).unwrap(), "gg-graph-v1").await;
    let x_ref = upload_input(&env, &r, &x).await;
    let y_ref = upload_input(&env, &r, &y).await;
    // Three rounds under retention: the round that produced the last checkpoint, and so the weights
    // it read, stay whole; the step before them can expire.
    let rounds = if memory == Memory::Pruned { 3 } else { 2 };
    let (batch, tasks, seed) = (16u32, 2u32, 11u64);
    let micro = (batch / tasks) as u64;

    // The machine: a third of what the whole model requires, without any lever.
    let whole = memory::task_memory(&model, &report, micro, Default::default()).total();
    let cap = whole / 3;
    let bounds = stages::partition(&model, &report, micro, cap).unwrap();
    assert!(bounds.len() > 3, "{bounds:?}");
    let mut workers = vec![
        setup_worker(&env, "e1").await,
        setup_worker(&env, "e2").await,
        setup_worker(&env, "e3").await,
    ];
    for w in &mut workers {
        w.device = cap;
    }
    let v = setup_validator(&env).await;
    let (s, run) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/collective", r.project),
            json!({
                "name": "stages", "graph": graph_ref, "datasets": {"ids": x_ref, "y": y_ref},
                "optimizer": "adam", "learning_rate": 0.01, "batch_per_round": batch,
                "tasks_per_round": tasks, "rounds": rounds, "seed": seed, "max_round_retries": 1,
                "stage_memory_bytes": cap,
            }),
        )
        .await;
    assert_eq!(s, 200, "{run}");
    assert_eq!(run["config"]["graph"]["stage_bounds"], json!(bounds));
    assert!(
        run["config"]["graph"]["task_device_bytes"]
            .as_u64()
            .unwrap()
            <= cap
    );
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    let forgetting = forget_meanwhile(&env, memory);
    let (view, submitted, decided) =
        drive(&env, &r, &run_id, &workers, &v, Duration::from_secs(300)).await;
    check_memory(&env, memory, forgetting);
    assert_eq!(view["state"], "completed", "{view}");
    let k = bounds.len() - 1;
    // Two waves per stage and per micro-batch, every round.
    assert_eq!(submitted, rounds as usize * 2 * k * tasks as usize);
    assert_eq!(decided, submitted);

    // No task required more than the workers' memory.
    let mut c = env.tdb.as_ref().unwrap().db.pool().acquire().await.unwrap();
    let (max_need,): (i64,) = sqlx::query_as(
        "SELECT max(required_device_bytes) FROM tasks WHERE compute_kind = 'graph.stage.v1'",
    )
    .fetch_one(&mut *c)
    .await
    .unwrap();
    drop(c);
    assert!(max_need as u64 <= cap && (max_need as u64) < whole);

    // Single-process replay: same stages, same aggregation.
    let prepared = Prepared::new(&model).unwrap();
    let resolved = gg_compute::graph::stage_task::resolve(model.clone(), &bounds).unwrap();
    let mut weights = exec::init_parameters(&model, seed);
    let mut opt = NamedOptimizerState::default();
    for step in 0..rounds as u64 {
        let round_seed = seed + step;
        let mut parts = Vec::new();
        for (start, count) in gg_control::workflows::collective::slices(batch, tasks) {
            let idx = round_indices(round_seed, total, start, count);
            let mut produced = BTreeMap::from([
                ("ids".to_string(), select_rows(&x, &idx)),
                ("y".to_string(), select_rows(&y, &idx)),
            ]);
            let ds = round_seed ^ start.rotate_left(32);
            for s in &resolved.stages {
                let only: BTreeMap<_, _> = s
                    .params
                    .iter()
                    .map(|n| (n.clone(), weights[n].clone()))
                    .collect();
                let ins: BTreeMap<_, _> = s
                    .inputs
                    .iter()
                    .map(|n| (n.clone(), produced[n].clone()))
                    .collect();
                let (o, _) = exec::stage_forward_cpu(&prepared, s, &only, &ins, ds).unwrap();
                produced.extend(o);
            }
            let mut received: BTreeMap<String, Vec<TensorF32>> = BTreeMap::new();
            let mut total_g: BTreeMap<String, TensorF32> = BTreeMap::new();
            for s in resolved.stages.iter().rev() {
                let only: BTreeMap<_, _> = s
                    .params
                    .iter()
                    .map(|n| (n.clone(), weights[n].clone()))
                    .collect();
                let ins: BTreeMap<_, _> = s
                    .inputs
                    .iter()
                    .map(|n| (n.clone(), produced[n].clone()))
                    .collect();
                let b = exec::stage_backward_cpu(&prepared, s, &only, &ins, &received, ds).unwrap();
                for (n, g) in b.input_gradients {
                    received.entry(n).or_default().push(g);
                }
                for (n, g) in as_sent(b.gradients) {
                    match total_g.get_mut(&n) {
                        Some(acc) => acc.data.iter_mut().zip(&g.data).for_each(|(a, b)| *a += *b),
                        None => {
                            total_g.insert(n, g);
                        }
                    }
                }
            }
            parts.push((count, total_g));
        }
        let g = aggregate_named(&parts).unwrap();
        apply_step_named(CoordOptim::Adam, 0.01, &mut weights, &g, &mut opt).unwrap();
    }
    let published = merged_checkpoint(&env, &r, &view).await;
    assert_eq!(published.len(), weights.len());
    for (name, t) in &weights {
        assert_eq!(
            &published[name], t,
            "{name}: staged grid run and replay differ"
        );
    }
    if memory == Memory::Pruned {
        check_retention(&env, &r, &run_id, &[graph_ref, x_ref, y_ref]).await;
    }
    env.stop().await;
}

/// D3 (docs/ROADMAP.md): a matrix larger than every worker. The plan sees it and proposes to tile
/// it; the server rewrites the description, tiles the initial weights uploaded parameter by
/// parameter, and the tiles train spread across stages. The published checkpoint, with tiles
/// reassembled, equals exactly a single-process replay.
#[tokio::test]
async fn a_matrix_larger_than_every_worker_is_spread_across_stages() {
    use gg_compute::grad::CoordOptim;
    use gg_compute::graph::exec::{self, Prepared};
    use gg_compute::graph::task::{
        NamedOptimizerState, aggregate_named, apply_step_named, round_indices, select_rows,
    };
    use gg_compute::graph::templates::{Positions, TransformerDims, transformer};
    use gg_compute::graph::tiling;

    let env = start().await;
    let r = setup_researcher(&env, 50_000).await;
    let (seq, vocab, total) = (8u64, 2048u64, 32u64);
    let model = transformer(TransformerDims {
        layers: 1,
        dim: 64,
        heads: 4,
        seq,
        vocab,
        ffn: 128,
        causal: true,
        positions: Positions::rotary(10000.0),
    })
    .unwrap();
    let biggest = model
        .parameters
        .iter()
        .map(|p| p.shape.iter().product::<u64>() * 4)
        .max()
        .unwrap();
    let mut d = gg_protocol::drbg::Drbg::new("e2e/tiles", 1);
    let ids = |d: &mut gg_protocol::drbg::Drbg| {
        TensorF32::new(
            vec![total, seq],
            (0..total * seq)
                .map(|_| d.next_index(vocab as u32) as f32)
                .collect(),
        )
        .unwrap()
    };
    let (x, y) = (ids(&mut d), ids(&mut d));
    let graph_ref =
        upload_bytes(&env, &r, serde_json::to_vec(&model).unwrap(), "gg-graph-v1").await;
    let x_ref = upload_input(&env, &r, &x).await;
    let y_ref = upload_input(&env, &r, &y).await;
    let seed = 13u64;
    let init = exec::init_parameters(&model, seed);
    let mut initial = BTreeMap::new();
    for (name, t) in &init {
        initial.insert(name.clone(), upload_input(&env, &r, t).await);
    }
    // Workers that can hold the largest matrix... only just not.
    let cap = biggest;
    let mut workers = vec![
        setup_worker(&env, "t1").await,
        setup_worker(&env, "t2").await,
    ];
    for w in &mut workers {
        w.device = cap;
    }
    let (s, _) = claim_with(
        &env,
        &workers[0],
        json!({"device_memory_bytes": cap, "ram_bytes": 64u64 << 30,
               "profile": gg_compute::graph::PROFILE_VERSION,
               "kinds": ["graph.grad.v1", "graph.stage.v1"]}),
    )
    .await;
    assert_eq!(s, 204);
    let (batch, tasks, rounds) = (4u32, 2u32, 2u32);
    let (_, plan) = env
        .post(
            &r.token,
            "/researcher/plan",
            json!({"graph": model, "batch_per_round": batch, "tasks_per_round": tasks}),
        )
        .await;
    let staged = &plan["staged"];
    assert!(plan["recommendation"].is_null(), "{plan}");
    let max = staged["tile_max_param_bytes"]
        .as_u64()
        .expect("tiling proposed");
    assert!(!staged["tiled"].as_array().unwrap().is_empty(), "{plan}");
    assert!(plan["advice"].as_str().unwrap().contains("tiled"), "{plan}");

    let v = setup_validator(&env).await;
    let (s, run) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/collective", r.project),
            json!({
                "name": "tiles", "graph": graph_ref, "datasets": {"ids": x_ref, "y": y_ref},
                "optimizer": "adam", "learning_rate": 0.01, "batch_per_round": batch,
                "tasks_per_round": tasks, "rounds": rounds, "seed": seed, "max_round_retries": 1,
                "stage_memory_bytes": cap, "tile_max_param_bytes": max,
                "initial_parameters": initial,
            }),
        )
        .await;
    assert_eq!(s, 200, "{run}");
    let tiling: Vec<tiling::Tiled> =
        serde_json::from_value(run["config"]["graph"]["tiling"].clone()).unwrap();
    assert!(!tiling.is_empty());
    let bounds: Vec<u32> =
        serde_json::from_value(run["config"]["graph"]["stage_bounds"].clone()).unwrap();
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    let (view, _, _) = drive(&env, &r, &run_id, &workers, &v, Duration::from_secs(300)).await;
    assert_eq!(view["state"], "completed", "{view}");

    // Replay: same tiling, same stages, in a single process.
    let (tiled_model, done) = tiling::auto_tile(&model, max).unwrap();
    assert_eq!(done, tiling);
    let prepared = Prepared::new(&tiled_model).unwrap();
    let resolved = gg_compute::graph::stage_task::resolve(tiled_model.clone(), &bounds).unwrap();
    // The tiles of one matrix are indeed on different stages.
    let t = &tiling[0];
    let holders: std::collections::BTreeSet<usize> = (0..t.tiles)
        .map(|i| {
            let n = tiling::tile_name(&t.param, i, t.tiles);
            resolved
                .stages
                .iter()
                .position(|s| s.params.contains(&n))
                .unwrap()
        })
        .collect();
    assert!(holders.len() > 1, "{holders:?}");
    let mut weights = tiling::split_weights(&init, &tiling).unwrap();
    let mut opt = NamedOptimizerState::default();
    for step in 0..rounds as u64 {
        let round_seed = seed + step;
        let mut parts = Vec::new();
        for (start, count) in gg_control::workflows::collective::slices(batch, tasks) {
            let idx = round_indices(round_seed, total, start, count);
            let mut produced = BTreeMap::from([
                ("ids".to_string(), select_rows(&x, &idx)),
                ("y".to_string(), select_rows(&y, &idx)),
            ]);
            let ds = round_seed ^ start.rotate_left(32);
            for s in &resolved.stages {
                let only: BTreeMap<_, _> = s
                    .params
                    .iter()
                    .map(|n| (n.clone(), weights[n].clone()))
                    .collect();
                let ins: BTreeMap<_, _> = s
                    .inputs
                    .iter()
                    .map(|n| (n.clone(), produced[n].clone()))
                    .collect();
                let (o, _) = exec::stage_forward_cpu(&prepared, s, &only, &ins, ds).unwrap();
                produced.extend(o);
            }
            let mut received: BTreeMap<String, Vec<TensorF32>> = BTreeMap::new();
            let mut total_g: BTreeMap<String, TensorF32> = BTreeMap::new();
            for s in resolved.stages.iter().rev() {
                let only: BTreeMap<_, _> = s
                    .params
                    .iter()
                    .map(|n| (n.clone(), weights[n].clone()))
                    .collect();
                let ins: BTreeMap<_, _> = s
                    .inputs
                    .iter()
                    .map(|n| (n.clone(), produced[n].clone()))
                    .collect();
                let b = exec::stage_backward_cpu(&prepared, s, &only, &ins, &received, ds).unwrap();
                for (n, g) in b.input_gradients {
                    received.entry(n).or_default().push(g);
                }
                for (n, g) in as_sent(b.gradients) {
                    match total_g.get_mut(&n) {
                        Some(acc) => acc.data.iter_mut().zip(&g.data).for_each(|(a, b)| *a += *b),
                        None => {
                            total_g.insert(n, g);
                        }
                    }
                }
            }
            parts.push((count, total_g));
        }
        let g = aggregate_named(&parts).unwrap();
        apply_step_named(CoordOptim::Adam, 0.01, &mut weights, &g, &mut opt).unwrap();
    }
    let published =
        tiling::merge_weights(&merged_checkpoint(&env, &r, &view).await, &tiling).unwrap();
    let replay = tiling::merge_weights(&weights, &tiling).unwrap();
    assert_eq!(published.len(), model.parameters.len());
    for (name, t) in &replay {
        assert_eq!(&published[name], t, "{name}");
    }
    env.stop().await;
}

/// Functions and `Formula` (ADR-0025) through a distributed run: the server inlines the submitted
/// description, keeps the original as the source, the workers train the flattened version, and
/// the published checkpoint equals exactly a single-process replay.
#[tokio::test]
async fn a_description_with_functions_is_inlined_and_trains() {
    use gg_compute::grad::CoordOptim;
    use gg_compute::graph::exec::{self, Prepared};
    use gg_compute::graph::task::{
        NamedOptimizerState, aggregate_named, apply_step_named, round_indices, select_rows,
    };
    use gg_protocol::tensor_file::BundleF32;

    // A `mish(x·W + b)` layer written once, called twice; mish is a `Formula`.
    const GRAPH: &str = r#"{
      "format": "gg-graph/v1", "name": "e2e-functions",
      "inputs": [{"name": "x", "shape": [0, 6]}],
      "parameters": [
        {"name": "w1", "shape": [6, 10], "init": "xavier_uniform"},
        {"name": "b1", "shape": [10], "init": "zeros"},
        {"name": "w2", "shape": [10, 10], "init": "xavier_uniform"},
        {"name": "b2", "shape": [10], "init": "zeros"},
        {"name": "w3", "shape": [10, 3], "init": "xavier_uniform"}
      ],
      "functions": [
        {"name": "layer", "inputs": ["h", "w", "b"], "output": "a",
         "nodes": [
           {"op": "MatMul", "inputs": ["h", "w"], "output": "m"},
           {"op": "Add", "inputs": ["m", "b"], "output": "z"},
           {"op": "Formula", "inputs": ["z"], "output": "a", "attrs": {"expr": "a * tanh(softplus(a))"}}
         ]}
      ],
      "nodes": [
        {"op": "fn:layer", "inputs": ["x", "w1", "b1"], "output": "h1"},
        {"op": "fn:layer", "inputs": ["h1", "w2", "b2"], "output": "h2"},
        {"op": "MatMul", "inputs": ["h2", "w3"], "output": "logits"}
      ],
      "loss": {"cross_entropy": {"prediction": "logits", "target": {"name": "y", "shape": [0], "dtype": "i64"}}}
    }"#;
    let submitted: gg_compute::graph::ModelGraphV1 = serde_json::from_str(GRAPH).unwrap();
    let flat = gg_compute::graph::functions::inline(&submitted).unwrap();
    let env = start().await;
    let r = setup_researcher(&env, 500).await;
    let total = 64u64;
    let mut d = gg_protocol::drbg::Drbg::new("e2e/functions", 1);
    let x = TensorF32::new(
        vec![total, 6],
        (0..total * 6).map(|_| d.next_sym_f32()).collect(),
    )
    .unwrap();
    let y = TensorF32::new(
        vec![total],
        (0..total).map(|_| d.next_index(3) as f32).collect(),
    )
    .unwrap();

    // The plan accepts the description as written.
    let (s, plan) = env
        .post(
            &r.token,
            "/researcher/plan",
            json!({"graph": submitted, "batch_per_round": 32, "tasks_per_round": 2}),
        )
        .await;
    assert_eq!(s, 200, "{plan}");

    let graph_ref = upload_bytes(&env, &r, GRAPH.as_bytes().to_vec(), "gg-graph-v1").await;
    let x_ref = upload_input(&env, &r, &x).await;
    let y_ref = upload_input(&env, &r, &y).await;
    let (s, run) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/collective", r.project),
            json!({
                "name": "functions", "graph": graph_ref, "datasets": {"x": x_ref, "y": y_ref},
                "optimizer": "adam", "learning_rate": 0.02, "batch_per_round": 32,
                "tasks_per_round": 2, "rounds": 3, "seed": 9, "max_round_retries": 1,
            }),
        )
        .await;
    assert_eq!(s, 200, "{run}");
    // The executed description is the flattened version; the submitted one remains the source.
    let source: ArtifactRef =
        serde_json::from_value(run["config"]["graph"]["source_graph"].clone()).unwrap();
    assert_eq!(source.hash, graph_ref.hash);
    let stored: ArtifactRef =
        serde_json::from_value(run["config"]["graph"]["graph"].clone()).unwrap();
    assert_ne!(stored.hash, graph_ref.hash);
    let (s, bytes) = env
        .get_bytes(
            &r.token,
            &format!(
                "/researcher/projects/{}/artifacts/{}",
                r.project, stored.hash
            ),
        )
        .await;
    assert_eq!(s, 200);
    let executed: gg_compute::graph::ModelGraphV1 = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(executed, flat);

    let run_id = run["run_id"].as_str().unwrap().to_owned();
    let workers = vec![
        setup_worker(&env, "f1").await,
        setup_worker(&env, "f2").await,
    ];
    let v = setup_validator(&env).await;
    let (view, _, _) = drive(&env, &r, &run_id, &workers, &v, Duration::from_secs(120)).await;
    assert_eq!(view["state"], "completed", "{view}");
    let ck: ArtifactRef =
        serde_json::from_value(view["checkpoint"]["weights"]["checkpoint"].clone()).unwrap();
    let (s, bytes) = env
        .get_bytes(
            &r.token,
            &format!("/researcher/projects/{}/artifacts/{}", r.project, ck.hash),
        )
        .await;
    assert_eq!(s, 200);
    let published = BundleF32::decode(&bytes, &Default::default())
        .unwrap()
        .entries;

    // Single-process replay, on the flattened description.
    let prepared = Prepared::new(&flat).unwrap();
    let mut weights = exec::init_parameters(&flat, 9);
    let mut state = NamedOptimizerState::default();
    for step in 0..3u64 {
        let seed = 9 + step;
        let mut parts = Vec::new();
        for (start, count) in gg_control::workflows::collective::slices(32, 2) {
            let idx = round_indices(seed, total, start, count);
            let batch = BTreeMap::from([
                ("x".to_string(), select_rows(&x, &idx)),
                ("y".to_string(), select_rows(&y, &idx)),
            ]);
            let res = exec::gradient_cpu(&prepared, &weights, &batch, 0).unwrap();
            parts.push((count, as_sent(res.gradients)));
        }
        let g = aggregate_named(&parts).unwrap();
        apply_step_named(CoordOptim::Adam, 0.02, &mut weights, &g, &mut state).unwrap();
    }
    assert_eq!(published.len(), weights.len());
    for (name, t) in &weights {
        let max = published[name]
            .data
            .iter()
            .zip(&t.data)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(max <= 1e-6, "{name}: deviation {max} from the replay");
    }
}

/// ADR-0026: the waves of a staged round are speculative. The next wave opens on **submitted**
/// outputs, not yet verified. Here a dishonest worker forges an activation in wave 0; wave 1
/// consumes it before any verification; the validator rejects the forgery. The round must be
/// reissued, and the published checkpoint must be **byte-identical** to that of an honest run with
/// the same seed: a fraud can cost a round, never a wrong weight.
#[tokio::test]
async fn a_forged_activation_already_passed_on_reissues_the_round() {
    use gg_compute::graph::stages;
    use gg_compute::graph::templates::{Positions, TransformerDims, transformer};

    let env = start().await;
    let r = setup_researcher(&env, 50_000).await;
    let (seq, vocab, total) = (8u64, 30u64, 32u64);
    let model = transformer(TransformerDims {
        layers: 2,
        dim: 16,
        heads: 2,
        seq,
        vocab,
        ffn: 32,
        causal: true,
        positions: Positions::rotary(10000.0),
    })
    .unwrap();
    let report = gg_compute::graph::validate(&model).unwrap();
    let bounds = stages::even_bounds(report.order.len(), 2);
    let mut d = gg_protocol::drbg::Drbg::new("e2e/speculation", 1);
    let ids = |d: &mut gg_protocol::drbg::Drbg| {
        TensorF32::new(
            vec![total, seq],
            (0..total * seq)
                .map(|_| d.next_index(vocab as u32) as f32)
                .collect(),
        )
        .unwrap()
    };
    let (x, y) = (ids(&mut d), ids(&mut d));
    let graph_ref =
        upload_bytes(&env, &r, serde_json::to_vec(&model).unwrap(), "gg-graph-v1").await;
    let x_ref = upload_input(&env, &r, &x).await;
    let y_ref = upload_input(&env, &r, &y).await;
    let workers = vec![
        setup_worker(&env, "s1").await,
        setup_worker(&env, "s2").await,
        setup_worker(&env, "s3").await,
    ];
    let v = setup_validator(&env).await;
    let create = |name: &str| {
        json!({
            "name": name, "graph": graph_ref, "datasets": {"ids": x_ref, "y": y_ref},
            "optimizer": "adam", "learning_rate": 0.01, "batch_per_round": 8,
            "tasks_per_round": 2, "rounds": 1, "seed": 21, "max_round_retries": 2,
            "stage_bounds": bounds,
        })
    };

    // Run A: the fraud.
    let (s, run) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/collective", r.project),
            create("fraud"),
        )
        .await;
    assert_eq!(s, 200, "{run}");
    let run_a = run["run_id"].as_str().unwrap().to_owned();
    // Wave 0: one forged task, one honest. Nothing is verified.
    let bad = worker_execute_with(&env, &workers[0], Mode::Corrupt)
        .await
        .unwrap();
    let forged: Vec<String> = bad
        .submission
        .body
        .outputs
        .iter()
        .filter(|(n, _)| n.starts_with("out."))
        .map(|(_, a)| a.hash.to_hex())
        .collect();
    assert_eq!(worker_submit(&env, &workers[0], &bad).await.0, 200);
    let good = worker_execute(&env, &workers[1]).await.unwrap();
    assert_eq!(worker_submit(&env, &workers[1], &good).await.0, 200);
    // Without any decision, wave 1 opens (the coordinator wakes up on submission) and reads the
    // forged activation.
    let started = Instant::now();
    let mut consumed_forgery = false;
    while started.elapsed() < Duration::from_secs(20) && !consumed_forgery {
        for w in &workers[1..] {
            if let Some(e) = worker_execute(&env, w).await {
                consumed_forgery |= e.inputs.iter().any(|h| forged.contains(h));
                assert_eq!(worker_submit(&env, w, &e).await.0, 200);
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        consumed_forgery,
        "wave 1 should have opened on the submitted outputs"
    );
    // The validator decides: the forgery is rejected, the rest accepted.
    let mut rejected = 0;
    while let Some(outcome) = validator_verify_once(&env, &v).await {
        if outcome == VerificationOutcome::Rejected {
            rejected += 1;
        }
    }
    assert_eq!(rejected, 1);
    // Then everyone is honest until the end.
    let (view_a, _, _) = drive(&env, &r, &run_a, &workers, &v, Duration::from_secs(120)).await;
    assert_eq!(view_a["state"], "completed", "{view_a}");
    let attempts: Vec<(i64, String)> = view_a["rounds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| {
            (
                x["attempt"].as_i64().unwrap(),
                x["state"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(
        attempts.iter().any(|(a, s)| *a == 1 && s == "failed"),
        "{attempts:?}"
    );
    assert!(attempts.iter().any(|(a, _)| *a >= 2), "{attempts:?}");

    // Run B: the same, honest.
    let (s, run) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/collective", r.project),
            create("honest"),
        )
        .await;
    assert_eq!(s, 200, "{run}");
    let run_b = run["run_id"].as_str().unwrap().to_owned();
    let (view_b, _, _) = drive(&env, &r, &run_b, &workers, &v, Duration::from_secs(120)).await;
    assert_eq!(view_b["state"], "completed", "{view_b}");
    // Same bytes: same hashes for every shard of the checkpoint and of the Adam state.
    assert_eq!(view_a["checkpoint"], view_b["checkpoint"]);
}

/// Model and data of the hand-off-before-durable-copy tests: 2 stages, 1 round of 2 tasks.
async fn handoff_run(env: &Env, r: &Researcher, name: &str) -> (String, Value) {
    use gg_compute::graph::stages;
    use gg_compute::graph::templates::{Positions, TransformerDims, transformer};
    let (seq, vocab, total) = (8u64, 30u64, 32u64);
    let model = transformer(TransformerDims {
        layers: 2,
        dim: 16,
        heads: 2,
        seq,
        vocab,
        ffn: 32,
        causal: true,
        positions: Positions::rotary(10000.0),
    })
    .unwrap();
    let report = gg_compute::graph::validate(&model).unwrap();
    let bounds = stages::even_bounds(report.order.len(), 2);
    let mut d = gg_protocol::drbg::Drbg::new("e2e/handoff", 1);
    let ids = |d: &mut gg_protocol::drbg::Drbg| {
        TensorF32::new(
            vec![total, seq],
            (0..total * seq)
                .map(|_| d.next_index(vocab as u32) as f32)
                .collect(),
        )
        .unwrap()
    };
    let (x, y) = (ids(&mut d), ids(&mut d));
    let graph_ref = upload_bytes(env, r, serde_json::to_vec(&model).unwrap(), "gg-graph-v1").await;
    let x_ref = upload_input(env, r, &x).await;
    let y_ref = upload_input(env, r, &y).await;
    let (s, run) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/collective", r.project),
            json!({
                "name": name, "graph": graph_ref, "datasets": {"ids": x_ref, "y": y_ref},
                "optimizer": "adam", "learning_rate": 0.01, "batch_per_round": 8,
                "tasks_per_round": 2, "rounds": 1, "seed": 31, "max_round_retries": 2,
                "stage_bounds": bounds,
            }),
        )
        .await;
    assert_eq!(s, 200, "{run}");
    (run["run_id"].as_str().unwrap().to_owned(), run)
}

/// ADR-0026, bounded exception to invariant I7: an activation passes to the next stage **before**
/// its durable copy. The producer declares hash, size and chunk digests, then submits; the next
/// wave opens; the server answers "not durable yet" to whoever downloads it and signs a manifest
/// over the declared digests; **no verification starts** before the durable copy; uploading after
/// submission unblocks everything; the checkpoint is byte-identical to that of a run where
/// everything was uploaded before submitting.
#[tokio::test]
async fn an_activation_passes_on_before_its_durable_copy_without_changing_the_result() {
    let env = start().await;
    let r = setup_researcher(&env, 50_000).await;
    let workers = vec![
        setup_worker(&env, "h1").await,
        setup_worker(&env, "h2").await,
        setup_worker(&env, "h3").await,
    ];
    let v = setup_validator(&env).await;
    let (run_a, _) = handoff_run(&env, &r, "handoff").await;

    // Wave 0: both tasks declare their activations and submit without uploading them.
    let mut declared = Vec::new();
    for w in &workers[..2] {
        let e = worker_execute_with(&env, w, Mode::Declare).await.unwrap();
        assert!(!e.deferred.is_empty(), "a declared activation");
        assert_eq!(worker_submit(&env, w, &e).await.0, 200);
        declared.push(e);
    }
    // Nothing is verifiable: the outputs have no durable copy.
    assert!(validator_verify_once(&env, &v).await.is_none());

    // Wave 1 opens on these activations; a worker that claims it sees them as inputs.
    let pending_hash = declared[0].deferred[0]
        .0
        .split('/')
        .nth(5)
        .unwrap()
        .split('?')
        .next()
        .unwrap()
        .to_owned();
    let started = Instant::now();
    let claim = loop {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "wave 1 does not open"
        );
        let (s, c) = env
            .post(
                &workers[2].token,
                "/worker/claim",
                json!({
                    "kinds": ["graph.stage.v1"], "wait_secs": 1,
                    "ram_bytes": 64u64 << 30, "device_memory_bytes": workers[2].device,
                    "profile": gg_compute::graph::PROFILE_VERSION,
                }),
            )
            .await;
        if s == 200
            && c["inputs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["hash"] == pending_hash)
        {
            break c;
        }
        if s == 200 {
            let id = c["assignment"]["body"]["attempt_id"].as_str().unwrap();
            env.post(
                &workers[2].token,
                &format!("/worker/attempts/{id}/abandon"),
                json!({}),
            )
            .await;
        }
    };
    // Not on the server yet: 409, and a manifest signed by the server over the declared digests,
    // against which the reader will verify every chunk coming from the producer.
    let (s, _) = env
        .get_bytes(
            &workers[2].token,
            &format!("/worker/artifacts/{pending_hash}"),
        )
        .await;
    assert_eq!(s, 409);
    let (s, m) = env
        .get(
            &workers[2].token,
            &format!("/worker/artifacts/{pending_hash}/manifest"),
        )
        .await;
    assert_eq!(s, 200, "{m}");
    let manifest: Signed<ChunkManifestV1> = serde_json::from_value(m["manifest"].clone()).unwrap();
    manifest
        .verify(&env.network, Domain::ChunkManifest)
        .unwrap();
    assert_eq!(manifest.body.chunks, declared[0].deferred[0].2);
    // The producer may announce itself as holder of an activation it has not uploaded yet;
    // another worker may not. Authorization is checked before the address policy: this test server
    // refuses local addresses, so the authorized producer gets 422 and the stranger 404.
    let announce =
        json!({ "addr": "127.0.0.1:7300", "tls_fingerprint": Sha256Digest::of_bytes(b"k") });
    let path = format!("/worker/artifacts/{pending_hash}/announce");
    let (s, a) = env.post(&workers[0].token, &path, announce.clone()).await;
    assert_eq!(s, 422, "{a}");
    let (s, _) = env.post(&workers[1].token, &path, announce).await;
    assert_eq!(s, 404);
    let id = claim["assignment"]["body"]["attempt_id"].as_str().unwrap();
    env.post(
        &workers[2].token,
        &format!("/worker/attempts/{id}/abandon"),
        json!({}),
    )
    .await;

    // The durable copies arrive, after submission: everything unblocks.
    for (w, e) in workers[..2].iter().zip(&declared) {
        for (path, bytes, _) in &e.deferred {
            let (s, resp) = env.put_bytes(&w.token, path, bytes.clone()).await;
            assert_eq!(s, 200, "{resp}");
        }
    }
    let (view_a, _, _) = drive(&env, &r, &run_a, &workers, &v, Duration::from_secs(120)).await;
    assert_eq!(view_a["state"], "completed", "{view_a}");

    // The same run, everything uploaded before submitting: same bytes.
    let (run_b, _) = handoff_run(&env, &r, "classic").await;
    let (view_b, _, _) = drive(&env, &r, &run_b, &workers, &v, Duration::from_secs(120)).await;
    assert_eq!(view_b["state"], "completed", "{view_b}");
    assert_eq!(view_a["checkpoint"], view_b["checkpoint"]);
}

/// A declared activation whose durable copy never arrives poisons the round: it is reissued, and
/// the run still completes.
#[tokio::test]
async fn an_activation_never_stored_reissues_the_round() {
    let env = start_with(1).await;
    let r = setup_researcher(&env, 50_000).await;
    let workers = vec![
        setup_worker(&env, "p1").await,
        setup_worker(&env, "p2").await,
    ];
    let v = setup_validator(&env).await;
    let (run, _) = handoff_run(&env, &r, "lost").await;
    for w in &workers {
        let e = worker_execute_with(&env, w, Mode::Declare).await.unwrap();
        assert_eq!(worker_submit(&env, w, &e).await.0, 200);
    }
    // One-second deadline exceeded without a durable copy: the coordinator reissues the round.
    let started = Instant::now();
    loop {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the round is not reissued"
        );
        let (_, view) = env
            .get(&r.token, &format!("/researcher/collective/{run}"))
            .await;
        if view["rounds"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["attempt"] == 1 && x["state"] == "failed")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // As soon as the round's failure is visible, none of its tasks remains claimable: they are
    // canceled in the same transaction (otherwise a worker could still take a task of the poisoned
    // round, whose input will never arrive).
    let mut c = env.tdb.as_ref().unwrap().db.pool().acquire().await.unwrap();
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tasks t JOIN jobs j ON j.id = t.job_id
          WHERE j.name LIKE 'lost-round0-a1-%'
            AND t.state NOT IN ('accepted','rejected','failed','cancelled','submitted','inconclusive')",
    )
    .fetch_one(&mut *c)
    .await
    .unwrap();
    drop(c);
    assert_eq!(open, 0, "a task of the poisoned round is still claimable");
    let (view, _, _) = drive(&env, &r, &run, &workers, &v, Duration::from_secs(120)).await;
    assert_eq!(view["state"], "completed", "{view}");
}

/// The plan tiles an oversized matrix into tiles small enough for a stage to fit. Description
/// exported by the PyTorch driver (`Embedding` 64, `Linear` 1024 × 1024 of 4 MiB, head 64) on
/// 3 MB workers. A tile of a quarter of the machine never fitted on its own: a stage counts every
/// parameter several times (value, gradient, working copies). Found by
/// `gg-python/tests/test_live.py::test_a_matrix_larger_than_every_worker_is_split_on_the_grid`.
#[tokio::test]
async fn the_plan_tiles_a_matrix_small_enough_for_a_stage_to_fit() {
    let env = start().await;
    let r = setup_researcher(&env, 500).await;
    let model: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/big-inner-matrix.json")).unwrap();
    let cap = 3_000_000u64;
    let mut w = setup_worker(&env, "small").await;
    w.device = cap;
    let (s, _) = claim_with(
        &env,
        &w,
        json!({"device_memory_bytes": cap, "ram_bytes": 64u64 << 30,
               "profile": gg_compute::graph::PROFILE_VERSION,
               "kinds": ["graph.grad.v1", "graph.stage.v1"]}),
    )
    .await;
    assert_eq!(s, 204);
    let (s, plan) = env
        .post(
            &r.token,
            "/researcher/plan",
            json!({"graph": model, "batch_per_round": 32, "tasks_per_round": 8}),
        )
        .await;
    assert_eq!(s, 200, "{plan}");
    let staged = &plan["staged"];
    assert!(!staged.is_null(), "{plan}");
    let max = staged["tile_max_param_bytes"]
        .as_u64()
        .expect("tiles proposed");
    assert!(max < cap / 4, "{max}");
    assert!(
        staged["largest_stage_bytes"].as_u64().unwrap() <= cap,
        "{plan}"
    );
    assert!(
        staged["tiled"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t[0] == "mid.weight"),
        "{plan}"
    );
}

/// Withdrawing a dataset a collective run trains on stops the run: it is cancelled with the reason
/// as its failure, its open round is closed and nothing of it can be claimed any more.
#[tokio::test]
async fn withdrawing_a_training_dataset_stops_the_run() {
    let env = start().await;
    let r = setup_researcher(&env, 500).await;
    let (x, y) = dataset();
    let train_x = upload_input(&env, &r, &x).await;
    let train_y = upload_input(&env, &r, &y).await;
    let (s, run) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/collective", r.project),
            json!({
                "name": "withdrawn", "train_x": train_x, "train_y": train_y, "d_hidden": 12,
                "optimizer": "adam", "learning_rate": 0.02, "batch_per_round": 96,
                "tasks_per_round": 3, "rounds": 4, "seed": 5, "max_round_retries": 1,
            }),
        )
        .await;
    assert_eq!(s, 200, "{run}");
    let run_id = run["run_id"].as_str().unwrap().to_owned();

    let (s, out) = env
        .post(
            &env.admin,
            &format!("/admin/artifacts/{}/withdraw", train_y.hash),
            json!({ "reason": "labels under embargo" }),
        )
        .await;
    assert_eq!(s, 200, "{out}");
    assert_eq!(out["runs_cancelled"], 1, "{out}");

    let (s, view) = env
        .get(&r.token, &format!("/researcher/collective/{run_id}"))
        .await;
    assert_eq!(s, 200);
    assert_eq!(view["state"], "cancelled", "{view}");
    assert!(
        view["failure"]
            .as_str()
            .is_some_and(|f| f.contains("withdrawn") && f.contains("labels under embargo")),
        "{view}"
    );
    assert!(
        view["rounds"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["state"] != "open"),
        "{view}"
    );
    let w = setup_worker(&env, "w1").await;
    let (s, _) = claim_with(&env, &w, json!({})).await;
    assert_eq!(s, 204, "nothing of the run may be claimed");
    let (s, _) = env
        .post(
            &w.token,
            "/worker/claim",
            json!({ "kinds": ["mlp.grad.v1"], "wait_secs": 0 }),
        )
        .await;
    assert_eq!(s, 204, "nothing of the run may be claimed");
    env.stop().await;
}

async fn run_view(env: &Env, r: &Researcher, run_id: &str) -> Value {
    env.get(&r.token, &format!("/researcher/collective/{run_id}"))
        .await
        .1
}

/// A reference computed on the GPU (ADR-0031): a GPU validator's decision counts only once a CPU
/// validator has recomputed the result too, when the engine check is drawn (here, always). The
/// check is never offered to a GPU validator, must state the CPU profile, and a disagreement opens
/// a dispute instead of paying.
#[tokio::test]
async fn a_gpu_verified_result_waits_for_its_cpu_engine_check() {
    const GRAPH: &str = r#"{
      "format": "gg-graph/v1", "name": "engine-check",
      "inputs": [{"name": "x", "shape": [0, 6]}],
      "parameters": [
        {"name": "w1", "shape": [6, 10], "init": "xavier_uniform"},
        {"name": "w2", "shape": [10, 3], "init": "xavier_uniform"}
      ],
      "nodes": [
        {"op": "MatMul", "inputs": ["x", "w1"], "output": "h"},
        {"op": "Gelu", "inputs": ["h"], "output": "a"},
        {"op": "MatMul", "inputs": ["a", "w2"], "output": "logits"}
      ],
      "loss": {"cross_entropy": {"prediction": "logits", "target": {"name": "y", "shape": [0], "dtype": "i64"}}}
    }"#;
    let env = start_configured(|cfg| cfg.engine_check_rate = 1.0).await;
    let r = setup_researcher(&env, 500).await;
    let mut d = gg_protocol::drbg::Drbg::new("e2e/engine-check", 1);
    let x = TensorF32::new(vec![32, 6], (0..32 * 6).map(|_| d.next_sym_f32()).collect()).unwrap();
    let y = TensorF32::new(vec![32], (0..32).map(|_| d.next_index(3) as f32).collect()).unwrap();
    let graph_ref = upload_bytes(&env, &r, GRAPH.as_bytes().to_vec(), "gg-graph-v1").await;
    let x_ref = upload_input(&env, &r, &x).await;
    let y_ref = upload_input(&env, &r, &y).await;
    let (s, run) = env
        .post(
            &r.token,
            &format!("/researcher/projects/{}/collective", r.project),
            json!({
                "name": "engine-check", "graph": graph_ref, "datasets": {"x": x_ref, "y": y_ref},
                "optimizer": "sgd", "learning_rate": 0.1, "batch_per_round": 16,
                "tasks_per_round": 1, "rounds": 2, "seed": 3, "max_round_retries": 1,
            }),
        )
        .await;
    assert_eq!(s, 200, "{run}");
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    let w = setup_worker(&env, "w").await;
    let gpu = setup_validator(&env).await;
    let cpu = setup_validator(&env).await;

    // Round 1. The GPU validator decides the primary verification with the GPU profile.
    let e = worker_execute(&env, &w).await.expect("round 1 task");
    assert_eq!(worker_submit(&env, &w, &e).await.0, 200);
    let (s, resp, _) = validator_verify_as(
        &env,
        &gpu,
        Some(GPU_F32_WGPU_VULKAN_V1),
        GPU_F32_WGPU_VULKAN_V1,
        None,
    )
    .await
    .expect("primary");
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["outcome"], "accepted");
    assert_eq!(resp["applied"], false, "{resp}");
    assert_eq!(resp["awaiting"], 1, "an engine check is pending: {resp}");
    assert_eq!(
        run_view(&env, &r, &run_id).await["step"],
        0,
        "nothing aggregated before the CPU check"
    );
    // The engine check is not offered to a GPU validator.
    assert!(
        validator_verify_as(
            &env,
            &gpu,
            Some(GPU_F32_WGPU_VULKAN_V1),
            GPU_F32_WGPU_VULKAN_V1,
            None
        )
        .await
        .is_none()
    );
    // A CPU validator gets it; a decision claiming the GPU profile is refused, the CPU one counts.
    let work = claim_work(&env, &cpu, None).await.expect("engine check");
    let (s, resp, _) = decide_work(&env, &cpu, &work, GPU_F32_WGPU_VULKAN_V1, None).await;
    assert_eq!(s, 422, "{resp}");
    let (s, resp, _) = decide_work(&env, &cpu, &work, REFERENCE_PROFILE, None).await;
    assert_eq!(s, 200, "{resp}");
    assert_eq!(resp["applied"], true, "{resp}");
    assert_eq!(resp["task_state"], "accepted");

    // Round 2. The CPU engine check disagrees: a dispute, nothing paid, the round waits.
    let started = Instant::now();
    let e = loop {
        if let Some(e) = worker_execute(&env, &w).await {
            break e;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "round 2 never opened"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(worker_submit(&env, &w, &e).await.0, 200);
    // A CPU validator dedicated to engine checks is not offered the primary verification.
    let (s, resp) = env
        .post(
            &cpu.token,
            "/validator/claim",
            json!({ "wait_secs": 0, "secondary_only": true }),
        )
        .await;
    assert_eq!(s, 204, "{resp}");
    let (s, resp, _) = validator_verify_as(
        &env,
        &gpu,
        Some(GPU_F32_WGPU_VULKAN_V1),
        GPU_F32_WGPU_VULKAN_V1,
        None,
    )
    .await
    .expect("primary");
    assert_eq!(s, 200, "{resp}");
    let (s, resp, _) = validator_verify_as(
        &env,
        &cpu,
        None,
        REFERENCE_PROFILE,
        Some(VerificationOutcome::Rejected),
    )
    .await
    .expect("engine check");
    assert_eq!(s, 200, "{resp}");
    assert!(resp["dispute_id"].is_string(), "{resp}");
    assert!(resp["receipt_id"].is_null(), "{resp}");
    assert_eq!(run_view(&env, &r, &run_id).await["step"], 1);
    env.stop().await;
}

/// A task's gradient as the grid receives it: rounded to bf16, the default (ADR-0035). The server
/// averages these values in f32, so the replay rounds them the same way to match it bit for bit.
fn as_sent(g: BTreeMap<String, TensorF32>) -> BTreeMap<String, TensorF32> {
    use gg_protocol::tensor_file::{bf16_bits, bf16_value};
    g.into_iter()
        .map(|(n, mut t)| {
            t.data
                .iter_mut()
                .for_each(|x| *x = bf16_value(bf16_bits(*x)));
            (n, t)
        })
        .collect()
}
