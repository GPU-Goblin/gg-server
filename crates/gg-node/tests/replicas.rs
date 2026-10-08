//! Verification by other contributors for a job a lab node holds (ADR-0042), against a real
//! control plane and PostgreSQL: the node pulls both computations of each task and compares them;
//! an agreement it signs is taken as it is; a difference it reports counts only once a comparer,
//! a contributor of an account foreign to the task drawn by the server, finds the same (the server
//! reads none of the outputs); when the comparer finds otherwise, a second one settles it, and
//! whoever misreported, the node or the first comparer, is suspended pending a human.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::*;
use gg_client::cache::ArtifactCache;
use gg_client::p2p::{FetchConfig, PeerServer};
use gg_compute::engine::EngineSelection;
use gg_core::*;
use gg_protocol::domain::Domain;
use gg_protocol::keys::SecretKey;
use gg_protocol::messages::*;
use gg_protocol::signed::Signed;
use gg_protocol::tensor_file::TensorF32;
use serde_json::json;

/// A worker of its own contributor account that serves its outputs.
struct Contributor {
    w: Worker,
    server: PeerServer,
    cache: ArtifactCache,
    dir: tempfile::TempDir,
}

async fn contributor(env: &Env) -> Contributor {
    let w = worker(env).await;
    join(env, w.id, uuid::Uuid::now_v7()).await;
    let api = gg_client::client::Api::new_allowing_plain_http(&env.base).unwrap();
    let info = api.info().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cache = ArtifactCache::new(dir.path().join("cache"), u64::MAX);
    let server = PeerServer::start(
        "127.0.0.1:0",
        cache.clone(),
        info.network_id,
        info.server_public_key,
        8,
        u64::MAX,
    )
    .await
    .unwrap();
    Contributor {
        w,
        server,
        cache,
        dir,
    }
}

/// Claim, fetch the node-held inputs through peers, compute (adding `corrupt` to the first
/// element: 0 is honest), keep, declare and announce the output, submit. `false`: nothing offered.
async fn work(env: &Env, h: &Contributor, corrupt: f32) -> bool {
    let (s, claim) = env
        .post(
            &h.w.token,
            "/worker/claim",
            json!({ "kinds": ["tensor.matmul.v1"], "wait_secs": 0, "serves_peers": true }),
        )
        .await;
    if s == 204 {
        return false;
    }
    assert_eq!(s, 200, "{claim}");
    let api = gg_client::client::Api::new_allowing_plain_http(&env.base).unwrap();
    let info = api.info().await.unwrap();
    let assignment: Signed<AssignmentV1> =
        serde_json::from_value(claim["assignment"].clone()).unwrap();
    let spec: TaskSpecV1 = serde_json::from_value(claim["spec"].clone()).unwrap();
    let task = gg_compute::validate_definition(&spec.definition).unwrap();
    let attempt = assignment.body.attempt_id;
    let dir = h.dir.path().join(attempt.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    let mut inputs = BTreeMap::new();
    for (name, r) in &spec.definition.inputs {
        let dest = dir.join(format!("in-{name}"));
        gg_client::p2p::fetch_with_peers(
            &api,
            &h.w.token,
            &h.w.key,
            &info.network_id,
            &info.server_public_key,
            r,
            &format!("/worker/artifacts/{}", r.hash),
            &dest,
            &FetchConfig {
                allow_private_peers: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        inputs.insert(name.clone(), dest);
    }
    let mut report =
        gg_compute::matmul::execute(EngineSelection::FlexCpu, &task, &inputs, &dir.join("out"))
            .unwrap();
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
        h.cache.store(&f.hash, &f.path).unwrap();
        let chunks: Vec<Sha256Digest> = bytes
            .chunks(info.p2p_chunk_bytes as usize)
            .map(Sha256Digest::of_bytes)
            .collect();
        let (s, body) = env
            .post(
                &h.w.token,
                &format!("/worker/attempts/{attempt}/outputs/{}/declare", f.hash),
                json!({ "size": f.size, "chunks": chunks }),
            )
            .await;
        assert_eq!(s, 200, "{body}");
        let (s, body) = env
            .post(
                &h.w.token,
                &format!("/worker/artifacts/{}/announce", f.hash),
                json!({ "addr": h.server.addr().to_string(), "tls_fingerprint": h.server.fingerprint() }),
            )
            .await;
        assert_eq!(s, 200, "{body}");
    }
    let body = ResultSubmissionV1 {
        protocol_version: gg_protocol::PROTOCOL_VERSION,
        network: info.network_id.clone(),
        assignment_hash: assignment
            .body_hash(&info.network_id, Domain::Assignment)
            .unwrap(),
        task_id: assignment.body.task_id,
        attempt_id: attempt,
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
    let signed = Signed::sign(&h.w.key, &info.network_id, Domain::Result, body).unwrap();
    let (s, body) = env
        .post(
            &h.w.token,
            &format!("/worker/attempts/{attempt}/result"),
            serde_json::to_value(&signed).unwrap(),
        )
        .await;
    assert_eq!(s, 200, "{body}");
    true
}

/// A node-held matmul job whose outputs the node pulls; the node's key is returned too.
async fn setup(
    env: &Env,
    dir: &std::path::Path,
) -> (std::sync::Arc<gg_node::Node>, SecretKey, ProjectId) {
    let (token, project) = researcher(env, "Ada").await;
    let key = SecretKey::generate().unwrap();
    let node = start_node_keyed(
        env,
        &token,
        project,
        &dir.join("node"),
        Duration::from_secs(30),
        SecretKey::from_bytes(&key.to_bytes()),
    )
    .await;
    let mut d = gg_protocol::drbg::Drbg::new("replicas", 1);
    let a = TensorF32::new(vec![32, 16], d.sym_f32_vec(32 * 16)).unwrap();
    let b = TensorF32::new(vec![16, 8], d.sym_f32_vec(16 * 8)).unwrap();
    let mut refs = Vec::new();
    for (i, t) in [&a, &b].into_iter().enumerate() {
        let path = dir.join(format!("in{i}"));
        std::fs::write(&path, t.encode()).unwrap();
        refs.push(node.hold(project, &path, "ggt1-f32").await.unwrap());
    }
    let def = gg_compute::catalog::matmul_definition(32, 16, 8, refs[0], refs[1]).unwrap();
    let (s, job) = env
        .post(
            &token,
            &format!("/researcher/projects/{project}/jobs"),
            json!({ "name": "mm", "tasks": [def], "output_holder": node.id() }),
        )
        .await;
    assert_eq!(s, 200, "{job}");
    (node, key, project)
}

async fn states(env: &Env) -> Vec<(String, i16)> {
    let mut c = env.db().pool().acquire().await.unwrap();
    sqlx::query_as::<_, (String, i16)>(
        "SELECT state, replica_seq FROM tasks WHERE compare_family IS NULL
          ORDER BY replica_seq, created_at",
    )
    .fetch_all(&mut *c)
    .await
    .unwrap()
}

/// Pull what is to pull, compare what is to compare, until `done` holds.
async fn drive<F, Fut>(env: &Env, node: &gg_node::Node, what: &str, mut done: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let started = std::time::Instant::now();
    while !done().await {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "timed out waiting for {what}: {:?}",
            states(env).await
        );
        node.pull_once().await.unwrap();
        node.compare_once(0).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn start_env() -> impl std::future::Future<Output = Env> {
    start_with(|c| {
        c.replica_verification = true;
        c.spot_check_rate = 0.0;
        c.probation_tasks = 0;
        c.node_relay_max_bytes = 0;
        c.node_relay_wait_secs = 5;
    })
}

#[tokio::test]
async fn the_node_compares_two_computations_that_agree() {
    let env = start_env().await;
    let dir = tempfile::tempdir().unwrap();
    let (node, _, _) = setup(&env, dir.path()).await;
    let (p, q) = (contributor(&env).await, contributor(&env).await);
    assert!(work(&env, &p, 0.0).await);
    assert!(work(&env, &q, 0.0).await);
    drive(&env, &node, "both accepted", || async {
        states(&env).await == vec![("accepted".into(), 0), ("accepted".into(), 1)]
    })
    .await;
    let mut c = env.db().pool().acquire().await.unwrap();
    let (comparer, replayed): (String, Option<String>) =
        sqlx::query_as("SELECT comparer, replayed_outcome FROM comparisons")
            .fetch_one(&mut *c)
            .await
            .unwrap();
    assert_eq!(comparer, format!("node:{}", node.id()));
    assert_eq!(replayed, None, "an agreement is not replayed");
    drop(c);
    for h in [p, q] {
        h.server.stop().await;
    }
    node.stop().await;
    env.stop().await;
}

/// Claim a comparison (`replica.compare.v1`), read both computations' outputs from the node
/// through peers, compare, upload the verdict (flipped when `lie`), submit. `false`: nothing
/// offered.
async fn compare(env: &Env, h: &Contributor, lie: bool) -> bool {
    let (s, claim) = env
        .post(
            &h.w.token,
            "/worker/claim",
            json!({ "kinds": ["replica.compare.v1"], "wait_secs": 0 }),
        )
        .await;
    if s == 204 {
        return false;
    }
    assert_eq!(s, 200, "{claim}");
    let api = gg_client::client::Api::new_allowing_plain_http(&env.base).unwrap();
    let info = api.info().await.unwrap();
    let assignment: Signed<AssignmentV1> =
        serde_json::from_value(claim["assignment"].clone()).unwrap();
    let spec: TaskSpecV1 = serde_json::from_value(claim["spec"].clone()).unwrap();
    let task = gg_compute::validate_definition(&spec.definition).unwrap();
    let attempt = assignment.body.attempt_id;
    let dir = h.dir.path().join(attempt.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    let mut inputs = BTreeMap::new();
    for (name, r) in &spec.definition.inputs {
        let dest = dir.join(format!("in-{name}"));
        gg_client::p2p::fetch_with_peers(
            &api,
            &h.w.token,
            &h.w.key,
            &info.network_id,
            &info.server_public_key,
            r,
            &format!("/worker/artifacts/{}", r.hash),
            &dest,
            &FetchConfig {
                allow_private_peers: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        inputs.insert(name.clone(), dest);
    }
    let mut report =
        gg_compute::execute_task(EngineSelection::FlexCpu, &task, &inputs, &dir.join("out"))
            .unwrap();
    if lie {
        let v = report.outputs.get_mut("verdict").unwrap();
        let mut verdict: gg_compute::replica::CompareVerdict =
            serde_json::from_slice(&std::fs::read(&v.path).unwrap()).unwrap();
        verdict.outcome = match verdict.outcome {
            VerificationOutcome::Accepted => VerificationOutcome::Rejected,
            _ => VerificationOutcome::Accepted,
        };
        let bytes = serde_json::to_vec(&verdict).unwrap();
        std::fs::write(&v.path, &bytes).unwrap();
        v.hash = Sha256Digest::of_bytes(&bytes);
        v.size = bytes.len() as u64;
    }
    for f in report.outputs.values() {
        let r = reqwest::Client::new()
            .put(format!(
                "{}/v1/worker/attempts/{attempt}/outputs/{}?size={}",
                env.base, f.hash, f.size
            ))
            .bearer_auth(&h.w.token)
            .body(std::fs::read(&f.path).unwrap())
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    }
    let body = ResultSubmissionV1 {
        protocol_version: gg_protocol::PROTOCOL_VERSION,
        network: info.network_id.clone(),
        assignment_hash: assignment
            .body_hash(&info.network_id, Domain::Assignment)
            .unwrap(),
        task_id: assignment.body.task_id,
        attempt_id: attempt,
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
    let signed = Signed::sign(&h.w.key, &info.network_id, Domain::Result, body).unwrap();
    let (s, body) = env
        .post(
            &h.w.token,
            &format!("/worker/attempts/{attempt}/result"),
            serde_json::to_value(&signed).unwrap(),
        )
        .await;
    assert_eq!(s, 200, "{body}");
    true
}

/// `(comparer, replayed_outcome, replayed_by)` of the one comparison.
async fn comparison(env: &Env) -> (String, Option<String>, Option<String>) {
    let mut c = env.db().pool().acquire().await.unwrap();
    sqlx::query_as("SELECT comparer, replayed_outcome, replayed_by FROM comparisons ORDER BY created_at LIMIT 1")
        .fetch_one(&mut *c)
        .await
        .unwrap()
}

async fn suspended(env: &Env, worker: WorkerId) -> bool {
    let mut c = env.db().pool().acquire().await.unwrap();
    sqlx::query_scalar(
        "SELECT k.suspended_at IS NOT NULL FROM workers w JOIN contributors k ON k.id = w.contributor_id WHERE w.id = $1",
    )
    .bind(worker.0)
    .fetch_one(&mut *c)
    .await
    .unwrap()
}

async fn node_suspended(env: &Env, node: NodeId) -> bool {
    let mut c = env.db().pool().acquire().await.unwrap();
    sqlx::query_scalar("SELECT suspended_at IS NOT NULL FROM lab_nodes WHERE id = $1")
        .bind(node.0)
        .fetch_one(&mut *c)
        .await
        .unwrap()
}

/// Until a comparer's task is offered to `h`, then do it.
async fn compare_when_offered(env: &Env, node: &gg_node::Node, h: &Contributor, lie: bool) {
    let started = std::time::Instant::now();
    while !compare(env, h, lie).await {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "no comparison offered: {:?}",
            states(env).await
        );
        node.pull_once().await.unwrap();
        node.compare_once(0).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn a_difference_the_node_finds_counts_once_a_contributor_found_it_too() {
    let env = start_env().await;
    let dir = tempfile::tempdir().unwrap();
    let (node, _, _) = setup(&env, dir.path()).await;
    let (p, liar, third, checker) = (
        contributor(&env).await,
        contributor(&env).await,
        contributor(&env).await,
        contributor(&env).await,
    );
    assert!(work(&env, &p, 0.0).await);
    assert!(work(&env, &liar, 1.0).await);
    // The node finds a difference; a comparer of another account checks it.
    compare_when_offered(&env, &node, &checker, false).await;
    drive(&env, &node, "a third computation", || async {
        states(&env).await.len() == 3
    })
    .await;
    let (by, replayed, replayed_by) = comparison(&env).await;
    assert_eq!(by, format!("node:{}", node.id()));
    assert_eq!(replayed.as_deref(), Some("rejected"));
    assert_eq!(replayed_by, Some(format!("worker:{}", checker.w.id)));
    assert!(work(&env, &third, 0.0).await);
    drive(&env, &node, "the majority", || async {
        let s = states(&env).await;
        s[0].0 == "accepted" && s[2].0 == "accepted"
    })
    .await;
    assert_eq!(states(&env).await[1].0, "cancelled");
    for h in [p, liar, third, checker] {
        h.server.stop().await;
    }
    node.stop().await;
    env.stop().await;
}

#[tokio::test]
async fn a_node_that_misreports_a_comparison_is_suspended() {
    let env = start_env().await;
    let dir = tempfile::tempdir().unwrap();
    let (node, key, _) = setup(&env, dir.path()).await;
    let (p, q, c1, c2) = (
        contributor(&env).await,
        contributor(&env).await,
        contributor(&env).await,
        contributor(&env).await,
    );
    assert!(work(&env, &p, 0.0).await);
    assert!(work(&env, &q, 0.0).await);
    // The node pulls both, then claims they differ, to pay neither.
    let api = gg_client::client::Api::new_allowing_plain_http(&env.base).unwrap();
    let info = api.info().await.unwrap();
    let nt = api
        .login(&key, &info.network_id, "node-login")
        .await
        .unwrap()
        .token;
    let asked = loop {
        node.pull_once().await.unwrap();
        let (s, w) = env.get(&nt, "/node/comparisons").await;
        assert_eq!(s, 200, "{w}");
        if let Some(first) = w.as_array().and_then(|a| a.first()).cloned() {
            break first;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let side = |s: &serde_json::Value| ComparedV1 {
        task_id: serde_json::from_value(s["task_id"].clone()).unwrap(),
        attempt_id: serde_json::from_value(s["attempt_id"].clone()).unwrap(),
        submission_hash: serde_json::from_value(s["submission_hash"].clone()).unwrap(),
    };
    let lie = ComparisonV1 {
        protocol_version: gg_protocol::PROTOCOL_VERSION,
        network: info.network_id.clone(),
        family: serde_json::from_value(asked["family"].clone()).unwrap(),
        a: side(&asked["a"]),
        b: side(&asked["b"]),
        outcome: VerificationOutcome::Rejected,
        checks: Vec::new(),
        compared_at: UnixMillis::now(),
    };
    // Signed by another key: refused.
    let other = SecretKey::generate().unwrap();
    let forged = Signed::sign(&other, &info.network_id, Domain::Comparison, lie.clone()).unwrap();
    let (s, e) = env
        .post(
            &nt,
            "/node/comparisons",
            serde_json::to_value(&forged).unwrap(),
        )
        .await;
    assert_eq!(s, 422, "{e}");
    let signed = Signed::sign(&key, &info.network_id, Domain::Comparison, lie).unwrap();
    let (s, e) = env
        .post(
            &nt,
            "/node/comparisons",
            serde_json::to_value(&signed).unwrap(),
        )
        .await;
    assert_eq!(s, 200, "{e}");
    // A comparer finds they agree; a second one too: the node misreported.
    compare_when_offered(&env, &node, &c1, false).await;
    compare_when_offered(&env, &node, &c2, false).await;
    let started = std::time::Instant::now();
    while states(&env).await[..2] != [("accepted".into(), 0), ("accepted".into(), 1)] {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{:?}",
            states(&env).await
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(comparison(&env).await.1.as_deref(), Some("accepted"));
    assert!(node_suspended(&env, node.id()).await);
    assert!(!suspended(&env, c1.w.id).await && !suspended(&env, c2.w.id).await);
    // Its comparisons are no longer taken.
    let (s, _) = env
        .post(
            &nt,
            "/node/comparisons",
            serde_json::to_value(&signed).unwrap(),
        )
        .await;
    assert_eq!(s, 403);
    for h in [p, q, c1, c2] {
        h.server.stop().await;
    }
    node.stop().await;
    env.stop().await;
}

#[tokio::test]
async fn a_comparer_that_misreports_is_suspended() {
    let env = start_env().await;
    let dir = tempfile::tempdir().unwrap();
    let (node, _, _) = setup(&env, dir.path()).await;
    let (p, liar, c1, c2) = (
        contributor(&env).await,
        contributor(&env).await,
        contributor(&env).await,
        contributor(&env).await,
    );
    assert!(work(&env, &p, 0.0).await);
    assert!(work(&env, &liar, 1.0).await);
    // The node rightly finds a difference; the first comparer says they agree; the second settles.
    compare_when_offered(&env, &node, &c1, true).await;
    compare_when_offered(&env, &node, &c2, false).await;
    drive(&env, &node, "a third computation", || async {
        states(&env).await.len() == 3
    })
    .await;
    assert!(
        suspended(&env, c1.w.id).await,
        "the lying comparer's account"
    );
    assert!(!node_suspended(&env, node.id()).await);
    let (_, replayed, replayed_by) = comparison(&env).await;
    assert_eq!(replayed.as_deref(), Some("rejected"));
    assert_eq!(replayed_by, Some(format!("worker:{}", c2.w.id)));
    for h in [p, liar, c1, c2] {
        h.server.stop().await;
    }
    node.stop().await;
    env.stop().await;
}
