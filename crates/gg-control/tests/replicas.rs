//! Verification by other contributors (ADR-0042), against a real control plane and PostgreSQL:
//! every task is computed a second time by a worker of another contributor account, drawn by the
//! server; the two results are compared; two that agree are both paid, a replica that lies is
//! outvoted by a third computation and not paid, and three that all differ go to a human.

use std::collections::BTreeMap;
use std::path::PathBuf;

use gg_compute::engine::EngineSelection;
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
    tdb: Option<TestDb>,
    running: Option<Running>,
    http: reqwest::Client,
    base: String,
    admin: String,
    network: NetworkId,
    _tmp: tempfile::TempDir,
}

async fn start() -> Env {
    start_with(0.0, 0).await
}

/// With spot checks at `spot_check_rate`, and `probation_tasks`.
async fn start_with(spot_check_rate: f64, probation_tasks: u32) -> Env {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("gg_control=info,error")
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
        replica_verification: true,
        pairing_reserve_secs: 60,
        spot_check_rate,
        probation_tasks,
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

/// Claim, compute (adding `corrupt` to the first element of the output: 0 is honest), upload and
/// submit. `None` when nothing is offered to this worker.
async fn worker_run(env: &Env, w: &WorkerCtx, corrupt: f32) -> Option<Value> {
    let (s, claim) = env
        .post(
            &w.token,
            "/worker/claim",
            json!({ "kinds": ["tensor.matmul.v1"], "wait_secs": 0 }),
        )
        .await;
    if s == 204 {
        return None;
    }
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
    if corrupt != 0.0 {
        let c = report.outputs.get_mut("c").unwrap();
        let mut t = TensorF32::read_from_path(&c.path, &Default::default()).unwrap();
        t.data[0] += corrupt;
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
    Some(claim)
}

impl Env {
    fn db(&self) -> &gg_storage::Db {
        &self.tdb.as_ref().unwrap().db
    }
}

/// Wait (the coordinator runs on each submission, and every 2 s) until `done` holds.
async fn until<F, Fut>(what: &str, mut done: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let started = std::time::Instant::now();
    while !done().await {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Attach `w` to the contributor account `account` (created if new).
async fn join(env: &Env, w: &WorkerCtx, account: uuid::Uuid) {
    let mut c = env.db().pool().acquire().await.unwrap();
    if gg_storage::queries::contributors::get(&mut c, account)
        .await
        .unwrap()
        .is_none()
    {
        gg_storage::queries::contributors::create(
            &mut c,
            account,
            &format!("{account}@example.org"),
        )
        .await
        .unwrap();
    }
    assert!(
        gg_storage::queries::contributors::attach_worker(&mut c, w.id, account)
            .await
            .unwrap()
    );
}

/// (task state, replica_seq, worker of its accepted attempt) of every computation.
async fn computations(env: &Env) -> Vec<(String, i16)> {
    let mut c = env.db().pool().acquire().await.unwrap();
    sqlx::query_as::<_, (String, i16)>(
        "SELECT state, replica_seq FROM tasks ORDER BY replica_seq, created_at",
    )
    .fetch_all(&mut *c)
    .await
    .unwrap()
}

/// (worker, role, amount) of every receipt.
async fn receipts(env: &Env) -> Vec<(uuid::Uuid, String, i64)> {
    let mut c = env.db().pool().acquire().await.unwrap();
    sqlx::query_as::<_, (uuid::Uuid, String, i64)>(
        "SELECT worker_id, role, amount FROM receipts ORDER BY role",
    )
    .fetch_all(&mut *c)
    .await
    .unwrap()
}

fn account() -> uuid::Uuid {
    uuid::Uuid::now_v7()
}

#[tokio::test]
async fn two_contributors_that_agree_are_both_paid() {
    let env = start().await;
    let r = setup_researcher(&env, 1000).await;
    let (a, b) = (account(), account());
    let a1 = setup_worker(&env, "a1").await;
    let a2 = setup_worker(&env, "a2").await;
    let b1 = setup_worker(&env, "b1").await;
    join(&env, &a1, a).await;
    join(&env, &a2, a).await;
    join(&env, &b1, b).await;
    submit_job(&env, &r, 1).await;

    // The first computation, by account A; its replica exists at once.
    worker_run(&env, &a1, 0.0).await.expect("a task for a1");
    assert_eq!(
        computations(&env).await,
        vec![("submitted".into(), 0), ("queued".into(), 1)]
    );
    // Account A has computed this task: its other worker is not offered the replica.
    assert!(worker_run(&env, &a2, 0.0).await.is_none());
    // Another account computes it, not knowing it is a replica.
    let claim = worker_run(&env, &b1, 0.0)
        .await
        .expect("the replica for b1");
    assert_eq!(claim["assignment"]["body"]["role"], "primary");
    until("both accepted", || async {
        computations(&env).await == vec![("accepted".into(), 0), ("accepted".into(), 1)]
    })
    .await;
    // Both paid, one as primary, one as replica; no validator was involved.
    let paid = receipts(&env).await;
    assert_eq!(paid.len(), 2, "{paid:?}");
    assert_eq!((paid[0].0, paid[0].1.as_str()), (a1.id.0, "primary"));
    assert_eq!((paid[1].0, paid[1].1.as_str()), (b1.id.0, "replica"));
    let mut c = env.db().pool().acquire().await.unwrap();
    let (verifications, comparisons): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM verifications), (SELECT count(*) FROM comparisons)",
    )
    .fetch_one(&mut *c)
    .await
    .unwrap();
    drop(c);
    assert_eq!((verifications, comparisons), (0, 1));
    env.stop().await;
}

#[tokio::test]
async fn a_replica_that_lies_is_outvoted_and_not_paid() {
    let env = start().await;
    let r = setup_researcher(&env, 1000).await;
    let honest = setup_worker(&env, "honest").await;
    let liar = setup_worker(&env, "liar").await;
    let third = setup_worker(&env, "third").await;
    for w in [&honest, &liar, &third] {
        join(&env, w, account()).await;
    }
    submit_job(&env, &r, 2).await;
    worker_run(&env, &honest, 0.0).await.expect("first");
    worker_run(&env, &liar, 1.0).await.expect("replica");
    // They differ: a third computation, by a third account.
    until("a third computation", || async {
        computations(&env).await.len() == 3
    })
    .await;
    assert!(worker_run(&env, &honest, 0.0).await.is_none());
    assert!(worker_run(&env, &liar, 0.0).await.is_none());
    worker_run(&env, &third, 0.0).await.expect("third");
    until("the majority accepted", || async {
        let c = computations(&env).await;
        c[0].0 == "accepted" && c[2].0 == "accepted"
    })
    .await;
    // The liar's computation is rejected, and not computed again: the task is settled.
    let c = computations(&env).await;
    assert_eq!(c[1].0, "cancelled", "{c:?}");
    let paid: Vec<uuid::Uuid> = receipts(&env).await.into_iter().map(|p| p.0).collect();
    assert!(paid.contains(&honest.id.0) && paid.contains(&third.id.0));
    assert!(!paid.contains(&liar.id.0), "the liar is not paid");
    env.stop().await;
}

#[tokio::test]
async fn the_replica_goes_first_to_the_worker_the_server_drew() {
    let env = start().await;
    let r = setup_researcher(&env, 1000).await;
    let first = setup_worker(&env, "first").await;
    let others = [setup_worker(&env, "x").await, setup_worker(&env, "y").await];
    join(&env, &first, account()).await;
    for w in &others {
        join(&env, w, account()).await;
        // Seen: they asked for work (none yet), announcing what they run.
        assert!(worker_run(&env, w, 0.0).await.is_none());
    }
    submit_job(&env, &r, 3).await;
    worker_run(&env, &first, 0.0).await.expect("first");
    let mut c = env.db().pool().acquire().await.unwrap();
    let drawn: uuid::Uuid =
        sqlx::query_scalar("SELECT designated_worker FROM tasks WHERE replica_seq = 1")
            .fetch_one(&mut *c)
            .await
            .unwrap();
    drop(c);
    let (chosen, other) = if drawn == others[0].id.0 {
        (&others[0], &others[1])
    } else {
        assert_eq!(drawn, others[1].id.0);
        (&others[1], &others[0])
    };
    // The other is not offered it during the reservation, whoever asks first.
    assert!(worker_run(&env, other, 0.0).await.is_none());
    worker_run(&env, chosen, 0.0)
        .await
        .expect("the drawn worker");
    env.stop().await;
}

#[tokio::test]
async fn three_computations_that_all_differ_go_to_a_human() {
    let env = start().await;
    let r = setup_researcher(&env, 1000).await;
    let ws = [
        setup_worker(&env, "p").await,
        setup_worker(&env, "q").await,
        setup_worker(&env, "s").await,
    ];
    for w in &ws {
        join(&env, w, account()).await;
    }
    submit_job(&env, &r, 4).await;
    worker_run(&env, &ws[0], 0.0).await.expect("first");
    worker_run(&env, &ws[1], 1.0).await.expect("replica");
    until("a third computation", || async {
        computations(&env).await.len() == 3
    })
    .await;
    worker_run(&env, &ws[2], 2.0).await.expect("third");
    until("disputes", || async {
        computations(&env)
            .await
            .iter()
            .all(|(s, _)| s == "disputed")
    })
    .await;
    assert!(receipts(&env).await.is_empty(), "nothing paid");
    let (s, disputes) = env.get(&env.admin, "/admin/disputes").await;
    assert_eq!(s, 200, "{disputes}");
    let list = disputes.as_array().unwrap();
    assert_eq!(list.len(), 3, "{disputes}");
    // A human settles: the first computation was right, and it is paid.
    let mut c = env.db().pool().acquire().await.unwrap();
    let root_dispute: uuid::Uuid = sqlx::query_scalar(
        "SELECT d.id FROM disputes d JOIN tasks t ON t.id = d.task_id WHERE t.replica_seq = 0",
    )
    .fetch_one(&mut *c)
    .await
    .unwrap();
    drop(c);
    let (s, resolved) = env
        .post(
            &env.admin,
            &format!("/admin/disputes/{root_dispute}/resolve"),
            json!({ "outcome": "accepted", "reason": "recomputed by hand" }),
        )
        .await;
    assert_eq!(s, 200, "{resolved}");
    assert_eq!(resolved["task_state"], "accepted");
    assert_eq!(receipts(&env).await.len(), 1);
    env.stop().await;
}

/// Whether the contributor account of `w` is suspended.
async fn suspended(env: &Env, w: &WorkerCtx) -> bool {
    let mut c = env.db().pool().acquire().await.unwrap();
    sqlx::query_scalar(
        "SELECT k.suspended_at IS NOT NULL FROM workers w JOIN contributors k ON k.id = w.contributor_id WHERE w.id = $1",
    )
    .bind(w.id.0)
    .fetch_one(&mut *c)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_spot_checked_task_waits_for_its_third_computation() {
    // Every task drawn.
    let env = start_with(1.0, 0).await;
    let r = setup_researcher(&env, 1000).await;
    let ws = [
        setup_worker(&env, "p").await,
        setup_worker(&env, "q").await,
        setup_worker(&env, "s").await,
    ];
    for w in &ws {
        join(&env, w, account()).await;
    }
    submit_job(&env, &r, 5).await;
    worker_run(&env, &ws[0], 0.0).await.expect("first");
    // The third computation exists from the start.
    assert_eq!(computations(&env).await.len(), 3);
    worker_run(&env, &ws[1], 0.0).await.expect("replica");
    // The first two agree, but nothing is decided before the check.
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    assert!(receipts(&env).await.is_empty());
    worker_run(&env, &ws[2], 0.0).await.expect("check");
    until("all three accepted", || async {
        computations(&env)
            .await
            .iter()
            .all(|(s, _)| s == "accepted")
    })
    .await;
    assert_eq!(receipts(&env).await.len(), 3);
    env.stop().await;
}

#[tokio::test]
async fn a_colluding_pair_contradicted_by_a_spot_check_is_suspended() {
    let env = start_with(1.0, 0).await;
    let r = setup_researcher(&env, 1000).await;
    let ws = [
        setup_worker(&env, "p").await,
        setup_worker(&env, "q").await,
        setup_worker(&env, "check").await,
    ];
    for w in &ws {
        join(&env, w, account()).await;
    }
    submit_job(&env, &r, 6).await;
    // Two accounts return the same wrong result; the drawn check is honest.
    worker_run(&env, &ws[0], 1.0).await.expect("first");
    worker_run(&env, &ws[1], 1.0).await.expect("replica");
    worker_run(&env, &ws[2], 0.0).await.expect("check");
    until("disputes", || async {
        computations(&env)
            .await
            .iter()
            .all(|(s, _)| s == "disputed")
    })
    .await;
    assert!(suspended(&env, &ws[0]).await && suspended(&env, &ws[1]).await);
    assert!(
        !suspended(&env, &ws[2]).await,
        "the check is not sanctioned"
    );
    assert!(receipts(&env).await.is_empty(), "nothing paid");
    // A suspended account's machines get no more work.
    submit_job(&env, &r, 7).await;
    let (s, _) = env
        .post(
            &ws[0].token,
            "/worker/claim",
            json!({ "kinds": ["tensor.matmul.v1"], "wait_secs": 0 }),
        )
        .await;
    assert_ne!(s, 200);
    env.stop().await;
}

#[tokio::test]
async fn an_account_in_probation_has_its_tasks_spot_checked() {
    // No sample: only probation, for accounts with fewer than 2 accepted computations.
    let env = start_with(0.0, 2).await;
    let r = setup_researcher(&env, 1000).await;
    let (veteran, newcomer) = (setup_worker(&env, "v").await, setup_worker(&env, "n").await);
    join(&env, &veteran, account()).await;
    join(&env, &newcomer, account()).await;
    submit_job(&env, &r, 8).await;
    worker_run(&env, &veteran, 0.0).await.expect("first");
    assert_eq!(
        computations(&env).await.len(),
        3,
        "a new account: a third computation from the start"
    );
    env.stop().await;
}

#[tokio::test]
async fn a_busy_drawn_worker_does_not_hold_the_replica() {
    let env = start().await;
    let r = setup_researcher(&env, 1000).await;
    let (p, x, y) = (
        setup_worker(&env, "p").await,
        setup_worker(&env, "x").await,
        setup_worker(&env, "y").await,
    );
    for w in [&p, &x, &y] {
        join(&env, w, account()).await;
    }
    // x alone has announced itself: it is the one drawn for the first job's replica.
    assert!(worker_run(&env, &x, 0.0).await.is_none());
    submit_job(&env, &r, 9).await;
    submit_job(&env, &r, 10).await;
    let first = worker_run(&env, &p, 0.0).await.expect("first job's task");
    let family: uuid::Uuid = first["assignment"]["body"]["task_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    // x takes the second job's task and keeps it: it is busy.
    let (s, busy) = env
        .post(
            &x.token,
            "/worker/claim",
            json!({ "kinds": ["tensor.matmul.v1"], "wait_secs": 0 }),
        )
        .await;
    assert_eq!(s, 200, "{busy}");
    // The replica drawn for x does not wait for it: y takes it at once.
    let (s, got) = env
        .post(
            &y.token,
            "/worker/claim",
            json!({ "kinds": ["tensor.matmul.v1"], "wait_secs": 0 }),
        )
        .await;
    assert_eq!(s, 200, "{got}");
    let mut c = env.db().pool().acquire().await.unwrap();
    let (replica_of, designated): (Option<uuid::Uuid>, Option<uuid::Uuid>) =
        sqlx::query_as("SELECT replica_of, designated_worker FROM tasks WHERE id = $1")
            .bind(
                got["assignment"]["body"]["task_id"]
                    .as_str()
                    .unwrap()
                    .parse::<uuid::Uuid>()
                    .unwrap(),
            )
            .fetch_one(&mut *c)
            .await
            .unwrap();
    drop(c);
    assert_eq!(replica_of, Some(family));
    assert_eq!(designated, Some(x.id.0));
    env.stop().await;
}
