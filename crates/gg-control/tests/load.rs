//! Load and full cost of an accepted result (P8, scoping §17).
//!
//! This test is not run by `cargo xtask check`: it measures, it does not assert a property. It is
//! marked `#[ignore]` and is run explicitly:
//!
//! ```text
//! GG_LOAD_WORKERS=8 GG_LOAD_TASKS=64 GG_LOAD_DIM=128 \
//!   cargo test -p gg-control --test load -- --ignored --nocapture
//! ```
//!
//! The main indicator is the **full cost of an accepted result**: the execution compute, the
//! verification compute, the work lost to attempts that did not complete, and the credits spent.
//! It is neither a FLOPS throughput nor a worker count, and nothing here establishes an economic
//! advantage: see docs/ECONOMICS.md.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use gg_compute::engine::EngineSelection;
use gg_control::config::ServerConfig;
use gg_control::server::serve;
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

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Observed durations, to report a spread and not just a mean.
#[derive(Default)]
struct Samples(Vec<f64>);

impl Samples {
    fn push(&mut self, seconds: f64) {
        self.0.push(seconds);
    }
    fn quantile(&mut self, q: f64) -> f64 {
        if self.0.is_empty() {
            return 0.0;
        }
        self.0.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let idx = ((self.0.len() - 1) as f64 * q).round() as usize;
        self.0[idx]
    }
    fn sum(&self) -> f64 {
        self.0.iter().sum()
    }
    fn len(&self) -> usize {
        self.0.len()
    }
}

struct Api {
    http: reqwest::Client,
    base: String,
}

impl Api {
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
    async fn get_bytes(&self, token: &str, path: &str) -> Vec<u8> {
        self.http
            .get(format!("{}/v1{path}", self.base))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .to_vec()
    }
    async fn put_bytes(&self, token: &str, path: &str, bytes: Vec<u8>) -> u16 {
        self.http
            .put(format!("{}/v1{path}", self.base))
            .bearer_auth(token)
            .body(bytes)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }
    async fn login(&self, key: &SecretKey, net: &NetworkId, action: &str) -> String {
        let path = if action == "worker-login" {
            "/worker"
        } else {
            "/validator"
        };
        let (_s, ch) = self
            .post(
                "",
                &format!("{path}/challenge"),
                json!({ "public_key": key.public_key().to_hex() }),
            )
            .await;
        let nonce = hex::decode(ch["nonce"].as_str().unwrap()).unwrap();
        let body = LoginChallengeV1 {
            protocol_version: gg_protocol::PROTOCOL_VERSION,
            network: net.clone(),
            action: action.into(),
            nonce,
            public_key: key.public_key(),
            issued_at: UnixMillis::now(),
        };
        let signed = Signed::sign(key, net, Domain::Login, body).unwrap();
        let (_s, sess) = self
            .post(
                "",
                &format!("{path}/session"),
                serde_json::to_value(&signed).unwrap(),
            )
            .await;
        sess["token"].as_str().unwrap().to_owned()
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "load measurement: run explicitly with --ignored"]
async fn full_cost_of_an_accepted_result() {
    let workers = env_usize("GG_LOAD_WORKERS", 8);
    let task_count = env_usize("GG_LOAD_TASKS", 64);
    let dim = env_usize("GG_LOAD_DIM", 128) as u32;
    let validators = env_usize("GG_LOAD_VALIDATORS", 2);

    let tdb = TestDb::create_or_panic().await;
    let tmp = tempfile::tempdir().unwrap();
    let net = NetworkId::new("gg-load").unwrap();
    let cfg = ServerConfig {
        database_url: tdb.url(),
        data_dir: tmp.path().to_path_buf(),
        network_id: "gg-load".into(),
        claim_wait_max_secs: 2,
        quota_requests_per_minute: 600_000,
        quota_burst: 100_000,
        pool_size: 32,
        audit_rate: 0.0,
        ..ServerConfig::default()
    };
    let state = AppState::new(cfg, tdb.db.clone()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let running = serve(state, listener).await.unwrap();
    let api = Arc::new(Api {
        http: reqwest::Client::builder()
            .pool_max_idle_per_host(64)
            .build()
            .unwrap(),
        base: running.url(),
    });

    // Project, budget, inputs, and a job of `task_count` matrix products.
    let admin = gg_control::auth::new_token(gg_control::auth::API_TOKEN_PREFIX).unwrap();
    let mut c = tdb.db.pool().acquire().await.unwrap();
    identities::create_token(
        &mut c,
        &gg_control::auth::token_hash(&admin),
        "admin",
        None,
        "load",
        None,
    )
    .await
    .unwrap();
    drop(c);
    let (_s, r) = api
        .post(
            &admin,
            "/admin/researchers",
            json!({ "display_name": "Load", "admission": "approved" }),
        )
        .await;
    let rtoken = r["token"].as_str().unwrap().to_owned();
    let rid = r["researcher_id"].as_str().unwrap().to_owned();
    let (_s, p) = api
        .post(&admin, "/admin/projects", json!({ "name": "load", "allowed_kinds": ["tensor.matmul.v1"], "owner_researcher_id": rid, "data_policy_ack": true }))
        .await;
    let project = p["project_id"].as_str().unwrap().to_owned();
    api.post(
        &admin,
        &format!("/admin/projects/{project}/grant"),
        json!({ "amount": "1000000", "reference": "load" }),
    )
    .await;
    let mut d = gg_protocol::drbg::Drbg::new("load", 1);
    let a = TensorF32::new(
        vec![dim as u64, dim as u64],
        d.sym_f32_vec((dim * dim) as usize),
    )
    .unwrap();
    let bytes = a.encode();
    let hash = Sha256Digest::of_bytes(&bytes);
    let size = bytes.len() as u64;
    assert_eq!(
        api.put_bytes(
            &rtoken,
            &format!("/researcher/projects/{project}/artifacts/{hash}?size={size}&schema=ggt1-f32"),
            bytes,
        )
        .await,
        200
    );
    let aref = ArtifactRef { hash, size };
    let def = gg_compute::catalog::matmul_definition(dim, dim, dim, aref, aref).unwrap();
    let tasks: Vec<_> = (0..task_count).map(|_| def.clone()).collect();
    let started = Instant::now();
    let (s, job) = api
        .post(
            &rtoken,
            &format!("/researcher/projects/{project}/jobs"),
            json!({ "name": "load", "tasks": tasks }),
        )
        .await;
    assert_eq!(s, 200, "{job}");
    assert!(job["job_id"].is_string(), "{job}");

    // Shared counters: execution, verification, lost work, attempts.
    let exec_secs = Arc::new(std::sync::Mutex::new(Samples::default()));
    let verify_secs = Arc::new(std::sync::Mutex::new(Samples::default()));
    let claim_secs = Arc::new(std::sync::Mutex::new(Samples::default()));
    let attempts = Arc::new(AtomicU64::new(0));
    let lost = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for i in 0..workers {
        let (api, net) = (api.clone(), net.clone());
        let (admin, exec_secs, claim_secs) = (admin.clone(), exec_secs.clone(), claim_secs.clone());
        let (attempts, lost, done) = (attempts.clone(), lost.clone(), done.clone());
        handles.push(tokio::spawn(async move {
            let key = SecretKey::generate().unwrap();
            let (_s, reg) = api
                .post(
                    "",
                    "/worker/register",
                    json!({ "public_key": key.public_key().to_hex(), "display_name": format!("w{i}") }),
                )
                .await;
            let id = reg["worker_id"].as_str().unwrap().to_owned();
            api.post(
                &admin,
                &format!("/admin/workers/{id}/authorize"),
                json!({ "authorized": true }),
            )
            .await;
            let token = api.login(&key, &net, "worker-login").await;
            let dir = tempfile::tempdir().unwrap();
            loop {
                if done.load(Ordering::Relaxed) as usize >= task_count {
                    return;
                }
                let claim_started = Instant::now();
                let (s, claim) = api
                    .post(
                        &token,
                        "/worker/claim",
                        json!({ "kinds": ["tensor.matmul.v1"], "wait_secs": 1 }),
                    )
                    .await;
                if s == 204 {
                    continue;
                }
                if s != 200 {
                    lost.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                claim_secs
                    .lock()
                    .unwrap()
                    .push(claim_started.elapsed().as_secs_f64());
                attempts.fetch_add(1, Ordering::Relaxed);
                let assignment: Signed<AssignmentV1> =
                    serde_json::from_value(claim["assignment"].clone()).unwrap();
                let spec: TaskSpecV1 = serde_json::from_value(claim["spec"].clone()).unwrap();
                let task = gg_compute::validate_definition(&spec.definition).unwrap();
                let mut inputs = BTreeMap::new();
                for inp in claim["inputs"].as_array().unwrap() {
                    let name = inp["name"].as_str().unwrap();
                    let h = inp["hash"].as_str().unwrap();
                    let bytes = api.get_bytes(&token, &format!("/worker/artifacts/{h}")).await;
                    let path: PathBuf = dir.path().join(format!("{h}-{name}.ggt"));
                    if !path.exists() {
                        std::fs::write(&path, bytes).unwrap();
                    }
                    inputs.insert(name.to_owned(), path);
                }
                let out_dir = dir.path().join(format!("out-{}", assignment.body.attempt_id));
                let exec_started = Instant::now();
                let report =
                    gg_compute::matmul::execute(EngineSelection::FlexCpu, &task, &inputs, &out_dir)
                        .unwrap();
                exec_secs
                    .lock()
                    .unwrap()
                    .push(exec_started.elapsed().as_secs_f64());
                for f in report.outputs.values() {
                    let bytes = std::fs::read(&f.path).unwrap();
                    api.put_bytes(
                        &token,
                        &format!(
                            "/worker/attempts/{}/outputs/{}?size={}&schema=ggt1-f32",
                            assignment.body.attempt_id, f.hash, f.size
                        ),
                        bytes,
                    )
                    .await;
                }
                let body = ResultSubmissionV1 {
                    protocol_version: gg_protocol::PROTOCOL_VERSION,
                    network: net.clone(),
                    assignment_hash: assignment.body_hash(&net, Domain::Assignment).unwrap(),
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
                let signed = Signed::sign(&key, &net, Domain::Result, body).unwrap();
                let (s, _) = api
                    .post(
                        &token,
                        &format!("/worker/attempts/{}/result", signed.body.attempt_id),
                        serde_json::to_value(&signed).unwrap(),
                    )
                    .await;
                if s != 200 {
                    // Lease lost, generation superseded: discarded compute, counted as such.
                    lost.fetch_add(1, Ordering::Relaxed);
                }
                std::fs::remove_dir_all(&out_dir).ok();
            }
        }));
    }
    for i in 0..validators {
        let (api, net) = (api.clone(), net.clone());
        let (admin, verify_secs, done) = (admin.clone(), verify_secs.clone(), done.clone());
        handles.push(tokio::spawn(async move {
            let key = SecretKey::generate().unwrap();
            let (_s, v) = api
                .post(
                    &admin,
                    "/admin/validators",
                    json!({ "public_key": key.public_key().to_hex(), "display_name": format!("v{i}"), "operator": format!("op-{i}") }),
                )
                .await;
            let vid: ValidatorId = v["validator_id"].as_str().unwrap().parse().unwrap();
            let token = api.login(&key, &net, "validator-login").await;
            let dir = tempfile::tempdir().unwrap();
            loop {
                if done.load(Ordering::Relaxed) as usize >= task_count {
                    return;
                }
                let (s, work) = api.post(&token, "/validator/claim", json!({ "wait_secs": 1 })).await;
                if s != 200 {
                    continue;
                }
                let spec: TaskSpecV1 = serde_json::from_value(work["spec"].clone()).unwrap();
                let submission: Signed<ResultSubmissionV1> =
                    serde_json::from_value(work["submission"].clone()).unwrap();
                let task = gg_compute::validate_definition(&spec.definition).unwrap();
                let vidt = work["verification_id"].as_str().unwrap().to_owned();
                let mut inputs = BTreeMap::new();
                let mut outputs = BTreeMap::new();
                for (list, map) in [("inputs", &mut inputs), ("outputs", &mut outputs)] {
                    for item in work[list].as_array().unwrap() {
                        let name = item["name"].as_str().unwrap();
                        let h = item["hash"].as_str().unwrap();
                        let bytes = api
                            .get_bytes(&token, &format!("/validator/artifacts/{h}"))
                            .await;
                        let p = dir.path().join(format!("{vidt}-{list}-{name}"));
                        std::fs::write(&p, bytes).unwrap();
                        map.insert(name.to_owned(), p);
                    }
                }
                let verify_started = Instant::now();
                let verdict = gg_compute::verify_task(
                    &task,
                    &inputs,
                    &outputs,
                    &submission.body.outputs,
                    &submission.body.execution_profile.numeric_profile,
                )
                .unwrap();
                verify_secs
                    .lock()
                    .unwrap()
                    .push(verify_started.elapsed().as_secs_f64());
                let decision = VerificationDecisionV1 {
                    protocol_version: gg_protocol::PROTOCOL_VERSION,
                    network: net.clone(),
                    decision_id: DecisionId::new(),
                    submission_hash: submission.body_hash(&net, Domain::Result).unwrap(),
                    task_id: spec.task_id,
                    attempt_id: submission.body.attempt_id,
                    validator_id: vid,
                    method: "recompute-reference".into(),
                    method_version: 1,
                    reference_profile: gg_compute::profiles::REFERENCE_PROFILE.into(),
                    checks: verdict.checks,
                    artifacts_checked: verdict.artifacts_checked,
                    outcome: verdict.outcome,
                    decided_at: UnixMillis::now(),
                };
                let signed = Signed::sign(&key, &net, Domain::Verification, decision).unwrap();
                let (s, resp) = api
                    .post(
                        &token,
                        &format!("/validator/verifications/{vidt}/decision"),
                        serde_json::to_value(&signed).unwrap(),
                    )
                    .await;
                if s == 200 && resp["task_state"] == "accepted" {
                    done.fetch_add(1, Ordering::Relaxed);
                }
                let _ = std::fs::remove_dir_all(dir.path().join(&vidt));
            }
        }));
    }
    // Bounded wait: the measurement must not hang if something blocks.
    let deadline = Instant::now() + std::time::Duration::from_secs(600);
    while (done.load(Ordering::Relaxed) as usize) < task_count && Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let wall = started.elapsed().as_secs_f64();
    for h in handles {
        h.abort();
    }
    let accepted = done.load(Ordering::Relaxed) as usize;
    let (_s, budget) = api
        .post(
            &admin,
            &format!("/admin/projects/{project}/grant"),
            json!({ "amount": "0", "reference": "read" }),
        )
        .await;
    let spent: f64 = budget["spent"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);

    // The locks are released before any `await`: the measurement must not block itself.
    let (mut exec, mut verify, mut claim) = {
        let exec = std::mem::take(&mut *exec_secs.lock().unwrap());
        let verify = std::mem::take(&mut *verify_secs.lock().unwrap());
        let claim = std::mem::take(&mut *claim_secs.lock().unwrap());
        (exec, verify, claim)
    };
    let exec_total = exec.sum();
    let verify_total = verify.sum();
    let compute_total = exec_total + verify_total;
    println!("\n== load: {accepted}/{task_count} results accepted ==");
    println!("hardware: {} logical threads", num_threads());
    println!(
        "parameters: {workers} workers, {validators} validators, matmul {dim}³, CPU Flex profile"
    );
    println!(
        "wall clock: {wall:.2} s, i.e. {:.2} accepted results/s",
        accepted as f64 / wall
    );
    println!(
        "assignment: p50 {:.3} s, p95 {:.3} s ({} samples)",
        claim.quantile(0.5),
        claim.quantile(0.95),
        claim.len()
    );
    println!(
        "execution: p50 {:.3} s, p95 {:.3} s, total {exec_total:.2} s ({} attempts)",
        exec.quantile(0.5),
        exec.quantile(0.95),
        exec.len()
    );
    println!(
        "verification: p50 {:.3} s, p95 {:.3} s, total {verify_total:.2} s ({} recomputations)",
        verify.quantile(0.5),
        verify.quantile(0.95),
        verify.len()
    );
    println!(
        "lost work: {} submissions refused, {} attempts for {accepted} results",
        lost.load(Ordering::Relaxed),
        attempts.load(Ordering::Relaxed)
    );
    if accepted > 0 {
        println!(
            "\nMAIN INDICATOR — full cost of an accepted result:\n  \
             {:.3} s of compute ({:.3} s execution + {:.3} s verification), i.e. a verification \
             overhead of {:.0} %\n  {:.1} TC credits spent",
            compute_total / accepted as f64,
            exec_total / accepted as f64,
            verify_total / accepted as f64,
            100.0 * verify_total / exec_total.max(f64::EPSILON),
            spent / accepted as f64
        );
    }
    assert!(
        accepted > 0,
        "no result accepted: the measurement measured nothing"
    );
    running.stop().await;
    tdb.drop().await;
}

fn num_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0)
}
