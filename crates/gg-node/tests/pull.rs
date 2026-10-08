//! A lab node pulls the outputs of the jobs it holds (ADR-0041, A4), against a real control plane
//! and PostgreSQL. A worker that does not serve peers is never given such a job; one that does
//! keeps its large outputs, declares their chunk digests and serves them; the control plane
//! refuses them as uploads; the node pulls them over pinned TLS, checked against the manifest the
//! server signs, and becomes their holder. Outputs the node never pulls send the task again. A
//! validator reads node-held inputs and outputs through peers and decides (A5); when the node is
//! offline, it decides nothing and releases the verification.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::*;
use gg_client::cache::ArtifactCache;
use gg_client::p2p::{FetchConfig, PeerServer};
use gg_compute::engine::EngineSelection;
use gg_core::*;
use gg_protocol::domain::Domain;
use gg_protocol::messages::*;
use gg_protocol::signed::Signed;
use gg_protocol::tensor_file::TensorF32;
use serde_json::{Value, json};

/// A job of one matmul over two node-held inputs, whose outputs `node` pulls.
async fn node_job(
    env: &Env,
    token: &str,
    project: ProjectId,
    node: &gg_node::Node,
    dir: &std::path::Path,
) -> Value {
    let mut d = gg_protocol::drbg::Drbg::new("pull", 1);
    let a = TensorF32::new(vec![128, 192], d.sym_f32_vec(128 * 192)).unwrap();
    let b = TensorF32::new(vec![192, 128], d.sym_f32_vec(192 * 128)).unwrap();
    let mut refs = Vec::new();
    for (i, t) in [&a, &b].into_iter().enumerate() {
        let path = dir.join(format!("in{i}"));
        std::fs::write(&path, t.encode()).unwrap();
        refs.push(node.hold(project, &path, "ggt1-f32").await.unwrap());
    }
    let def = gg_compute::catalog::matmul_definition(128, 192, 128, refs[0], refs[1]).unwrap();
    let (s, job) = env
        .post(
            token,
            &format!("/researcher/projects/{project}/jobs"),
            json!({ "name": "mm", "tasks": [def], "output_holder": node.id() }),
        )
        .await;
    assert_eq!(s, 200, "{job}");
    job
}

async fn claim(env: &Env, w: &Worker, serves_peers: bool) -> (u16, Value) {
    env.post(
        &w.token,
        "/worker/claim",
        json!({ "kinds": ["tensor.matmul.v1"], "wait_secs": 1, "serves_peers": serves_peers }),
    )
    .await
}

/// What a worker holds after computing its task and serving the output.
struct Computed {
    server: PeerServer,
    cache: ArtifactCache,
    hash: Sha256Digest,
    bytes: Vec<u8>,
}

/// Fetch the inputs from the node, compute, keep the output in a chunk server, declare it,
/// announce it and submit the signed result, as `gg-worker` does for a job a lab node holds.
async fn compute_and_declare(
    env: &Env,
    w: &Worker,
    claim: &Value,
    dir: &std::path::Path,
) -> Computed {
    compute_and_declare_with(env, w, claim, dir, true).await
}

/// As [`compute_and_declare`]; the output is announced only if `announce` (otherwise nobody can
/// reach the worker for it, as behind a NAT it cannot open).
async fn compute_and_declare_with(
    env: &Env,
    w: &Worker,
    claim: &Value,
    dir: &std::path::Path,
    announce: bool,
) -> Computed {
    std::fs::create_dir_all(dir).unwrap();
    let api = gg_client::client::Api::new_allowing_plain_http(&env.base).unwrap();
    let info = api.info().await.unwrap();
    let assignment: Signed<AssignmentV1> =
        serde_json::from_value(claim["assignment"].clone()).unwrap();
    let spec: TaskSpecV1 = serde_json::from_value(claim["spec"].clone()).unwrap();
    let task = gg_compute::validate_definition(&spec.definition).unwrap();
    let mut inputs = BTreeMap::new();
    for (name, r) in &spec.definition.inputs {
        let dest = dir.join(format!("in-{name}"));
        gg_client::p2p::fetch_with_peers(
            &api,
            &w.token,
            &w.key,
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
    let report =
        gg_compute::matmul::execute(EngineSelection::FlexCpu, &task, &inputs, &dir.join("out"))
            .unwrap();
    let out = &report.outputs["c"];
    let bytes = std::fs::read(&out.path).unwrap();
    let attempt = assignment.body.attempt_id;

    // Too large for the control plane to relay (1 KiB in these tests).
    let r = reqwest::Client::new()
        .put(format!(
            "{}/v1/worker/attempts/{attempt}/outputs/{}?size={}",
            env.base, out.hash, out.size
        ))
        .bearer_auth(&w.token)
        .body(bytes.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 422, "{}", r.text().await.unwrap());

    let cache = ArtifactCache::new(dir.join("cache"), u64::MAX);
    cache.store(&out.hash, &out.path).unwrap();
    let server = PeerServer::start(
        "127.0.0.1:0",
        cache.clone(),
        info.network_id.clone(),
        info.server_public_key,
        8,
        u64::MAX,
    )
    .await
    .unwrap();
    let chunks: Vec<Sha256Digest> = bytes
        .chunks(info.p2p_chunk_bytes as usize)
        .map(Sha256Digest::of_bytes)
        .collect();
    let (s, body) = env
        .post(
            &w.token,
            &format!("/worker/attempts/{attempt}/outputs/{}/declare", out.hash),
            json!({ "size": out.size, "chunks": chunks }),
        )
        .await;
    assert_eq!(s, 200, "{body}");
    if announce {
        let (s, body) = env
            .post(
                &w.token,
                &format!("/worker/artifacts/{}/announce", out.hash),
                json!({ "addr": server.addr().to_string(), "tls_fingerprint": server.fingerprint() }),
            )
            .await;
        assert_eq!(s, 200, "{body}");
    }
    let submission = ResultSubmissionV1 {
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
    let signed = Signed::sign(&w.key, &info.network_id, Domain::Result, submission).unwrap();
    let (s, body) = env
        .post(
            &w.token,
            &format!("/worker/attempts/{attempt}/result"),
            serde_json::to_value(&signed).unwrap(),
        )
        .await;
    assert_eq!(s, 200, "{body}");
    Computed {
        server,
        cache,
        hash: out.hash,
        bytes,
    }
}

#[tokio::test]
async fn a_node_pulls_the_outputs_of_its_job_from_the_worker() {
    let env = start_with(|c| c.node_relay_max_bytes = 1024).await;
    let (token, project) = researcher(&env, "Ada").await;
    let dir = tempfile::tempdir().unwrap();
    let node = start_node(&env, &token, project, &dir.path().join("node")).await;
    let job = node_job(&env, &token, project, &node, dir.path()).await;

    let w = worker(&env).await;
    // A worker that does not serve its outputs is not given the job.
    let (s, body) = claim(&env, &w, false).await;
    assert_eq!(s, 204, "{body}");
    let (s, c) = claim(&env, &w, true).await;
    assert_eq!(s, 200, "{c}");
    assert_eq!(c["outputs_to_node"], json!(true));
    let computed = compute_and_declare(&env, &w, &c, dir.path()).await;

    assert_eq!(node.pull_once().await.unwrap(), 1);
    assert!(node.holds(&computed.hash).await);
    assert_eq!(
        std::fs::read(node.path_of(&computed.hash)).unwrap(),
        computed.bytes
    );
    // Two chunks of 64 KiB, both from the worker.
    assert_eq!(computed.server.stats().snapshot().0, 2);
    // Nothing waits any more, and the server keeps no copy.
    assert_eq!(node.pull_once().await.unwrap(), 0);
    let (s, st) = env
        .get(
            &token,
            &format!("/researcher/jobs/{}", job["job_id"].as_str().unwrap()),
        )
        .await;
    assert_eq!(s, 200, "{st}");
    assert_eq!(st["tasks"][0]["state"], json!("submitted"), "{st}");

    computed.server.stop().await;
    Arc::clone(&node).stop().await;
    env.stop().await;
}

#[tokio::test]
async fn outputs_the_node_never_pulls_send_the_task_again() {
    let env = start_with(|c| {
        c.node_relay_max_bytes = 1024;
        c.pending_output_secs = 1;
    })
    .await;
    let (token, project) = researcher(&env, "Ada").await;
    let dir = tempfile::tempdir().unwrap();
    let node = start_node(&env, &token, project, &dir.path().join("node")).await;
    node_job(&env, &token, project, &node, dir.path()).await;

    let first = worker(&env).await;
    let (s, c) = claim(&env, &first, true).await;
    assert_eq!(s, 200, "{c}");
    let task_id = c["assignment"]["body"]["task_id"].clone();
    let computed = compute_and_declare(&env, &first, &c, &dir.path().join("w1")).await;
    // The node never pulls. Past the deadline the task is queued again for another worker.
    let second = worker(&env).await;
    let mut again = None;
    for _ in 0..40 {
        let (s, c) = claim(&env, &second, true).await;
        if s == 200 {
            again = Some(c);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let again = again.expect("the task was not sent again");
    assert_eq!(again["assignment"]["body"]["task_id"], task_id);
    assert_eq!(again["assignment"]["body"]["generation"], json!(2));
    // The late output is no longer offered to the node.
    assert_eq!(node.pull_once().await.unwrap(), 0);

    computed.server.stop().await;
    node.stop().await;
    env.stop().await;
}

/// A real `gg-verifier`, registered by the operator.
async fn validator(env: &Env, dir: &std::path::Path) -> gg_verifier::Verifier {
    let v = gg_verifier::Verifier::new(gg_verifier::VerifierConfig {
        server_url: env.base.clone(),
        key_path: dir.join("validator.key"),
        work_dir: dir.join("validator"),
        claim_wait_secs: 2,
        allow_private_peers: true,
        ..Default::default()
    })
    .await
    .unwrap();
    let (s, body) = env
        .post(
            &env.admin,
            "/admin/validators",
            json!({ "public_key": v.public_key().to_hex(), "display_name": "v1" }),
        )
        .await;
    assert_eq!(s, 200, "{body}");
    v
}

#[tokio::test]
async fn a_validator_reads_node_held_inputs_and_outputs_through_peers() {
    let env = start_with(|c| c.node_relay_max_bytes = 1024).await;
    let (token, project) = researcher(&env, "Ada").await;
    let dir = tempfile::tempdir().unwrap();
    let node = start_node(&env, &token, project, &dir.path().join("node")).await;
    let job = node_job(&env, &token, project, &node, dir.path()).await;
    let w = worker(&env).await;
    let (s, c) = claim(&env, &w, true).await;
    assert_eq!(s, 200, "{c}");
    let computed = compute_and_declare(&env, &w, &c, &dir.path().join("w")).await;
    assert_eq!(node.pull_once().await.unwrap(), 1);

    // The control plane stores none of the three artifacts; the validator recomputes from the
    // node's inputs and compares with the node's copy of the output.
    let mut v = validator(&env, dir.path()).await;
    let (_tx, rx) = tokio::sync::watch::channel(false);
    v.run(rx, Some(1)).await.unwrap();
    assert_eq!(v.decided.len(), 1);
    assert_eq!(
        v.decided[0].outcome.to_string(),
        "accepted",
        "{:?}",
        v.decided[0].response
    );
    let (s, st) = env
        .get(
            &token,
            &format!("/researcher/jobs/{}", job["job_id"].as_str().unwrap()),
        )
        .await;
    assert_eq!(s, 200, "{st}");
    assert_eq!(st["tasks"][0]["state"], json!("accepted"), "{st}");

    computed.server.stop().await;
    node.stop().await;
    env.stop().await;
}

#[tokio::test]
async fn with_the_node_offline_a_validator_decides_nothing() {
    let env = start_with(|c| c.node_relay_max_bytes = 1024).await;
    let (token, project) = researcher(&env, "Ada").await;
    let dir = tempfile::tempdir().unwrap();
    let node = start_node(&env, &token, project, &dir.path().join("node")).await;
    let job = node_job(&env, &token, project, &node, dir.path()).await;
    let w = worker(&env).await;
    let (s, c) = claim(&env, &w, true).await;
    assert_eq!(s, 200, "{c}");
    let computed = compute_and_declare(&env, &w, &c, &dir.path().join("w")).await;
    assert_eq!(node.pull_once().await.unwrap(), 1);
    // Both the node and the worker go away: nobody serves the inputs or the output.
    node.stop().await;
    computed.server.stop().await;

    let mut v = validator(&env, dir.path()).await;
    let (_tx, rx) = tokio::sync::watch::channel(false);
    v.run(rx, Some(1)).await.unwrap();
    assert!(
        v.decided.is_empty(),
        "a decision was taken without the artifacts"
    );
    let (s, st) = env
        .get(
            &token,
            &format!("/researcher/jobs/{}", job["job_id"].as_str().unwrap()),
        )
        .await;
    assert_eq!(s, 200, "{st}");
    assert_eq!(st["tasks"][0]["state"], json!("submitted"), "{st}");
    env.stop().await;
}

#[tokio::test]
async fn a_node_that_cannot_pull_an_output_sends_its_task_again_at_once() {
    let env = start_with(|c| c.node_relay_max_bytes = 1024).await;
    let (token, project) = researcher(&env, "Ada").await;
    let dir = tempfile::tempdir().unwrap();
    let node = start_node_with(
        &env,
        &token,
        project,
        &dir.path().join("node"),
        std::time::Duration::ZERO,
    )
    .await;
    node_job(&env, &token, project, &node, dir.path()).await;
    let first = worker(&env).await;
    let (s, c) = claim(&env, &first, true).await;
    assert_eq!(s, 200, "{c}");
    let task_id = c["assignment"]["body"]["task_id"].clone();
    let computed = compute_and_declare(&env, &first, &c, &dir.path().join("w1")).await;
    // The producer vanishes before the node pulls: the node gives up at its first failure,
    // without waiting for the server's deadline (600 s here).
    computed.server.stop().await;
    let started = std::time::Instant::now();
    assert_eq!(node.pull_once().await.unwrap(), 0);
    // It asked the peers again for its whole request timeout (20 s) instead of falling back to a
    // control plane that will never hold the output.
    assert!(started.elapsed() >= std::time::Duration::from_secs(20));
    let second = worker(&env).await;
    let (s, again) = claim(&env, &second, true).await;
    assert_eq!(s, 200, "{again}");
    assert_eq!(again["assignment"]["body"]["task_id"], task_id);
    assert_eq!(again["assignment"]["body"]["generation"], json!(2));
    node.stop().await;
    env.stop().await;
}

#[tokio::test]
async fn a_worker_whose_output_was_not_pulled_is_not_given_the_task_again() {
    let env = start_with(|c| c.node_relay_max_bytes = 1024).await;
    let (token, project) = researcher(&env, "Ada").await;
    let dir = tempfile::tempdir().unwrap();
    let node = start_node_with(
        &env,
        &token,
        project,
        &dir.path().join("node"),
        std::time::Duration::ZERO,
    )
    .await;
    node_job(&env, &token, project, &node, dir.path()).await;
    let w = worker(&env).await;
    let (s, c) = claim(&env, &w, true).await;
    assert_eq!(s, 200, "{c}");
    let computed = compute_and_declare(&env, &w, &c, &dir.path().join("w")).await;
    computed.server.stop().await;
    assert_eq!(node.pull_once().await.unwrap(), 0);
    // Queued again, but not for this worker.
    let (s, body) = claim(&env, &w, true).await;
    assert_eq!(s, 204, "{body}");
    let other = worker(&env).await;
    let (s, body) = claim(&env, &other, true).await;
    assert_eq!(s, 200, "{body}");
    node.stop().await;
    env.stop().await;
}

#[tokio::test]
async fn a_worker_nobody_can_reach_pushes_its_output_to_the_node() {
    let env = start_with(|c| c.node_relay_max_bytes = 1024).await;
    let (token, project) = researcher(&env, "Ada").await;
    let dir = tempfile::tempdir().unwrap();
    let node = start_node(&env, &token, project, &dir.path().join("node")).await;
    node_job(&env, &token, project, &node, dir.path()).await;
    let w = worker(&env).await;
    let (s, c) = claim(&env, &w, true).await;
    assert_eq!(s, 200, "{c}");
    // Declared, announced nowhere: nobody can connect to the worker for it.
    let computed = compute_and_declare_with(&env, &w, &c, &dir.path().join("w"), false).await;

    // The worker waits for push requests and carries them out, as gg-worker's push loop does.
    let api = gg_client::client::Api::new_allowing_plain_http(&env.base).unwrap();
    let wtoken = w.token.clone();
    let cache = computed.cache.clone();
    let pusher = tokio::spawn(async move {
        for _ in 0..20 {
            let asked: Vec<gg_client::p2p::PushRequest> = api
                .get_json(&wtoken, "/worker/pushes?wait_secs=1")
                .await
                .unwrap();
            if let Some(req) = asked.into_iter().next() {
                return gg_client::p2p::push_artifact(
                    &cache,
                    &req,
                    std::time::Duration::from_secs(10),
                )
                .await;
            }
        }
        Err("never asked to push".to_owned())
    });
    // The node finds no one to read from, has it pushed, and holds it.
    assert_eq!(node.pull_once().await.unwrap(), 1);
    let chunks = computed.bytes.len().div_ceil(64 * 1024) as u32;
    assert_eq!(pusher.await.unwrap().unwrap(), chunks);
    assert!(node.holds(&computed.hash).await);
    assert_eq!(
        std::fs::read(node.path_of(&computed.hash)).unwrap(),
        computed.bytes
    );
    // Pulling it again is nothing to do: the output is the node's.
    assert_eq!(node.pull_once().await.unwrap(), 0);

    computed.server.stop().await;
    node.stop().await;
    env.stop().await;
}
