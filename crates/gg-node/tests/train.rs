//! A training run stepped by a lab node (ADR-0041, A6), against a real control plane, PostgreSQL
//! and `gg-verifier`: the node draws the initial weights, workers compute the rounds from what the
//! node holds and keep their gradients, the validator reads everything through peers, the node
//! pulls the accepted gradients and applies each optimizer step. The control plane stores the
//! description only; the trained checkpoint equals a single-process replay bit for bit.

mod common;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use common::*;
use gg_client::cache::ArtifactCache;
use gg_client::p2p::{FetchConfig, PeerServer};
use gg_compute::engine::EngineSelection;
use gg_core::*;
use gg_protocol::domain::Domain;
use gg_protocol::messages::*;
use gg_protocol::signed::Signed;
use gg_protocol::tensor_file::{BundleF32, TensorF32};
use serde_json::json;

const GRAPH: &str = r#"{
  "format": "gg-graph/v1", "name": "node-graph",
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

/// A hand-driven worker that keeps its outputs and serves them, as `gg-worker` does for a job a
/// lab node holds.
struct Holder {
    w: Worker,
    server: PeerServer,
    cache: ArtifactCache,
    dir: tempfile::TempDir,
}

async fn holder(env: &Env) -> Holder {
    let w = worker(env).await;
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
    Holder {
        w,
        server,
        cache,
        dir,
    }
}

/// Claim a task, fetch its inputs (through peers when the server has no copy), compute it on the
/// CPU, keep and declare every output, submit. Returns whether a task was done.
async fn work_once(env: &Env, h: &Holder) -> bool {
    work_once_with(env, h, false).await
}

/// As [`work_once`]; with `relay`, the outputs are uploaded to the control plane (small outputs,
/// which the server relays, as `gg-worker` does below its thresholds).
async fn work_once_with(env: &Env, h: &Holder, relay: bool) -> bool {
    let (s, claim) = env
        .post(
            &h.w.token,
            "/worker/claim",
            json!({ "kinds": ["graph.grad.v1", "graph.stage.v1"], "wait_secs": 0, "serves_peers": true,
                    "ram_bytes": 64u64 << 30, "profile": gg_compute::graph::PROFILE_VERSION }),
        )
        .await;
    if s == 204 {
        return false;
    }
    assert_eq!(s, 200, "{claim}");
    assert_eq!(claim["outputs_to_node"], json!(true));
    let api = gg_client::client::Api::new_allowing_plain_http(&env.base).unwrap();
    let info = api.info().await.unwrap();
    let assignment: Signed<AssignmentV1> =
        serde_json::from_value(claim["assignment"].clone()).unwrap();
    let spec: TaskSpecV1 = serde_json::from_value(claim["spec"].clone()).unwrap();
    let task = gg_compute::validate_definition(&spec.definition).unwrap();
    let attempt = assignment.body.attempt_id;
    let work = h.dir.path().join(attempt.to_string());
    std::fs::create_dir_all(&work).unwrap();
    let mut inputs = BTreeMap::new();
    for (name, r) in &spec.definition.inputs {
        let dest = work.join(format!("in-{name}"));
        // Produced here by an earlier wave: from the worker's own store, as gg-worker does.
        if h.cache.path_of(&r.hash).exists() {
            std::fs::copy(h.cache.path_of(&r.hash), &dest).unwrap();
            inputs.insert(name.clone(), dest);
            continue;
        }
        let got = gg_client::p2p::fetch_with_peers(
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
        .await;
        if let Err(e) = got {
            panic!("input {name} ({}): {e}", r.hash);
        }
        inputs.insert(name.clone(), dest);
    }
    let report =
        gg_compute::execute_task(EngineSelection::FlexCpu, &task, &inputs, &work.join("out"))
            .unwrap();
    for f in report.outputs.values() {
        let bytes = std::fs::read(&f.path).unwrap();
        if relay {
            let r = reqwest::Client::new()
                .put(format!(
                    "{}/v1/worker/attempts/{attempt}/outputs/{}?size={}",
                    env.base, f.hash, f.size
                ))
                .bearer_auth(&h.w.token)
                .body(bytes)
                .send()
                .await
                .unwrap();
            assert!(r.status().is_success(), "{}", r.text().await.unwrap());
            continue;
        }
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

/// A real validator running in the background for the whole test. Stopping it between passes
/// could cancel a claim the server had just answered, and that verification would stay leased to
/// it until its lease expires.
struct Validating {
    stop: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<gg_verifier::Verifier>,
}

fn validate_in_background(mut v: gg_verifier::Verifier) -> Validating {
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        v.run(rx, None).await.unwrap();
        v
    });
    Validating { stop, task }
}

impl Validating {
    async fn finish(self) -> gg_verifier::Verifier {
        let _ = self.stop.send(true);
        self.task.await.unwrap()
    }
}

fn dataset() -> (TensorF32, TensorF32) {
    let total = 96u64;
    let mut d = gg_protocol::drbg::Drbg::new("node/graph", 1);
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
    (x, y)
}

/// The same rounds replayed in one process from the same seeded weights, gradients rounded to
/// bf16 as the workers send them.
fn replay(x: &TensorF32, y: &TensorF32, rounds: u64) -> BTreeMap<String, TensorF32> {
    let graph: gg_compute::graph::ModelGraphV1 = serde_json::from_str(GRAPH).unwrap();
    replay_from(
        x,
        y,
        rounds,
        gg_compute::graph::exec::init_parameters(&graph, 5),
    )
}

/// The same, from given starting weights.
fn replay_from(
    x: &TensorF32,
    y: &TensorF32,
    rounds: u64,
    start: BTreeMap<String, TensorF32>,
) -> BTreeMap<String, TensorF32> {
    replay_graph(&serde_json::from_str(GRAPH).unwrap(), x, y, rounds, start)
}

/// The same, for another description (the tiled one).
fn replay_graph(
    graph: &gg_compute::graph::ModelGraphV1,
    x: &TensorF32,
    y: &TensorF32,
    rounds: u64,
    start: BTreeMap<String, TensorF32>,
) -> BTreeMap<String, TensorF32> {
    use gg_compute::grad::CoordOptim;
    use gg_compute::graph::exec::{self, Prepared};
    use gg_compute::graph::task::{
        NamedOptimizerState, aggregate_named, apply_step_named, dropout_seed, round_indices,
        select_rows,
    };
    use gg_protocol::tensor_file::{bf16_bits, bf16_value};
    let prepared = Prepared::new(graph).unwrap();
    let mut weights = start;
    let mut state = NamedOptimizerState::default();
    let total = x.shape[0];
    for step in 0..rounds {
        let seed = 5 + step;
        let mut parts = Vec::new();
        for (start, count) in gg_control::workflows::collective::slices(48, 3) {
            let idx = round_indices(seed, total, start, count);
            let batch = BTreeMap::from([
                ("x".to_string(), select_rows(x, &idx)),
                ("y".to_string(), select_rows(y, &idx)),
            ]);
            let p = GraphGradParamsV1 {
                graph_hash: String::new(),
                graph_bytes: 0,
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
            let mut g = exec::gradient_cpu(&prepared, &weights, &batch, dropout_seed(&p))
                .unwrap()
                .gradients;
            for t in g.values_mut() {
                t.data
                    .iter_mut()
                    .for_each(|v| *v = bf16_value(bf16_bits(*v)));
            }
            parts.push((count, g));
        }
        let g = aggregate_named(&parts).unwrap();
        apply_step_named(CoordOptim::Adam, 0.02, &mut weights, &g, &mut state).unwrap();
    }
    weights
}

#[tokio::test]
async fn a_lab_node_steps_a_training_run_that_matches_a_single_process_replay() {
    node_training(false, false).await;
}

/// The same with each task given its micro-batch rows, cut by the node (the server reads no
/// dataset), one dataset held in two parts.
#[tokio::test]
async fn a_lab_node_cuts_the_rows_of_each_task() {
    node_training(false, true).await;
}

/// The same with small gradients the server relays (the default below 64 MiB): the node reads
/// them from the server.
#[tokio::test]
async fn a_lab_node_steps_with_gradients_the_server_relayed() {
    node_training(true, false).await;
}

async fn node_training(relay: bool, rows: bool) {
    let env = if relay {
        // A deployment that lets the server carry small outputs (off by default).
        start_with(|c| c.node_relay_max_bytes = 64 << 20).await
    } else {
        start_with(|c| c.node_relay_max_bytes = 0).await
    };
    let (token, project) = researcher(&env, "Ada").await;
    let dir = tempfile::tempdir().unwrap();
    let key = gg_protocol::keys::SecretKey::generate().unwrap();
    let node = start_node_keyed(
        &env,
        &token,
        project,
        &dir.path().join("node"),
        Duration::from_secs(30),
        gg_protocol::keys::SecretKey::from_bytes(&key.to_bytes()),
    )
    .await;

    // The description is small: the server keeps it. The datasets stay on the node.
    let r = reqwest::Client::new()
        .put(format!(
            "{}/v1/researcher/projects/{project}/artifacts/{}?size={}&schema=gg-graph-v1",
            env.base,
            Sha256Digest::of_bytes(GRAPH.as_bytes()),
            GRAPH.len()
        ))
        .bearer_auth(&token)
        .body(GRAPH.as_bytes().to_vec())
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let graph_ref = ArtifactRef {
        hash: Sha256Digest::of_bytes(GRAPH.as_bytes()),
        size: GRAPH.len() as u64,
    };
    let (x, y) = dataset();
    let mut refs = BTreeMap::new();
    for (name, t) in [("x", &x), ("y", &y)] {
        let path = dir.path().join(name);
        std::fs::write(&path, t.encode()).unwrap();
        refs.insert(name, node.hold(project, &path, "ggt1-f32").await.unwrap());
    }
    let mut req = json!({
        "name": "node-graph", "graph": graph_ref, "datasets": {"x": refs["x"], "y": refs["y"]},
        "optimizer": "adam", "learning_rate": 0.02, "batch_per_round": 48, "tasks_per_round": 3,
        "rounds": 3, "seed": 5, "max_round_retries": 1, "holder_node": node.id(),
    });
    if rows {
        // x in two parts of consecutive rows; each task gets only its rows, cut by the node.
        let mut parts = Vec::new();
        for (k, range) in [(0u64, 40u64), (40, 96)].into_iter().enumerate() {
            let idx: Vec<usize> = (range.0 as usize..range.1 as usize).collect();
            let part = gg_compute::graph::task::select_rows(&x, &idx);
            let path = dir.path().join(format!("x{k}"));
            std::fs::write(&path, part.encode()).unwrap();
            parts.push(node.hold(project, &path, "ggt1-f32").await.unwrap());
        }
        req["datasets"] = json!({ "y": refs["y"] });
        req["dataset_parts"] = json!({ "x": parts });
        req["batch_inputs"] = json!(true);
    }
    // The server does not cut a node's model: it is sent the stage bounds the node computed.
    let mut staged = req.clone();
    staged["stage_memory_bytes"] = json!(1 << 20);
    let (s, e) = env
        .post(
            &token,
            &format!("/researcher/projects/{project}/collective"),
            staged,
        )
        .await;
    assert_eq!(s, 422, "{e}");
    assert!(e.to_string().contains("stage_memory_bytes"), "{e}");
    let (s, run) = env
        .post(
            &token,
            &format!("/researcher/projects/{project}/collective"),
            req,
        )
        .await;
    assert_eq!(s, 200, "{run}");
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    // No weights yet: the node draws them.
    assert_eq!(run["checkpoint"]["weights"], json!({}));
    assert_eq!(run["config"]["graph"]["batch_inputs"], json!(rows));
    if rows {
        // Rows the server would not open a round with.
        let api = gg_client::client::Api::new_allowing_plain_http(&env.base).unwrap();
        let info = api.info().await.unwrap();
        let nt = api
            .login(&key, &info.network_id, "node-login")
            .await
            .unwrap()
            .token;
        let path = format!("/node/runs/{run_id}/rows");
        let one = json!({ "x": refs["x"], "y": refs["y"] });
        for (body, status) in [
            (json!({ "step": 0, "rows": [one.clone()] }), 422),
            (
                json!({ "step": 0, "rows": [one.clone(), one.clone(), one.clone()] }),
                422,
            ),
            (json!({ "step": 2, "rows": [] }), 409),
        ] {
            let (s, e) = env.post(&nt, &path, body).await;
            assert_eq!(s, status, "{e}");
        }
    }

    let holders = vec![holder(&env).await, holder(&env).await];
    let v = gg_verifier::Verifier::new(gg_verifier::VerifierConfig {
        server_url: env.base.clone(),
        key_path: dir.path().join("validator.key"),
        work_dir: dir.path().join("validator"),
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
    let validating = validate_in_background(v);

    let started = Instant::now();
    let mut steps = 0;
    let view = loop {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "the run did not finish"
        );
        steps += node.step_once().await.unwrap();
        let mut worked = false;
        for h in &holders {
            worked |= work_once_with(&env, h, relay).await;
        }
        node.pull_once().await.unwrap();
        let (s, view) = env
            .get(&token, &format!("/researcher/collective/{run_id}"))
            .await;
        assert_eq!(s, 200, "{view}");
        if matches!(
            view["state"].as_str(),
            Some("completed" | "failed" | "cancelled")
        ) {
            break view;
        }
        if !worked {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    assert_eq!(view["state"], "completed", "{view}");
    assert_eq!(view["step"], 3);
    assert_eq!(steps, 4, "step 0 and three rounds");
    let v = validating.finish().await;
    assert!(
        v.decided
            .iter()
            .all(|d| d.outcome == VerificationOutcome::Accepted)
    );
    assert_eq!(v.decided.len(), 9);

    // The published checkpoint is on the node, and equals the replay bit for bit.
    let ck: ArtifactRef =
        serde_json::from_value(view["checkpoint"]["weights"]["checkpoint"].clone()).unwrap();
    assert!(node.holds(&ck.hash).await);
    let published = BundleF32::decode(
        &std::fs::read(node.path_of(&ck.hash)).unwrap(),
        &Default::default(),
    )
    .unwrap()
    .entries;
    let reference = replay(&x, &y, 3);
    assert_eq!(published.len(), reference.len());
    for (name, t) in &reference {
        assert_eq!(published[name].shape, t.shape, "{name}");
        assert!(
            published[name].data == t.data,
            "{name}: the node's checkpoint differs from the replay"
        );
    }
    // The server stores nothing of the run but its description.
    let (s, _) = env
        .get(
            &token,
            &format!("/researcher/projects/{project}/artifacts/{}", ck.hash),
        )
        .await;
    assert_ne!(s, 200);

    for h in holders {
        h.server.stop().await;
    }
    node.stop().await;
    env.stop().await;
}

/// A model no worker holds whole, stepped by a lab node: the node cut it into stages (the bounds
/// it computed within a third of the whole model's memory), cuts every micro-batch's rows, holds
/// every activation and gradient the stages exchange, and sums each shard's partial gradients in
/// production order before the optimizer step. The checkpoint equals a single-process replay of
/// the same stages bit for bit.
#[tokio::test]
async fn a_lab_node_steps_a_model_cut_into_stages() {
    use gg_compute::grad::CoordOptim;
    use gg_compute::graph::exec::{self, Prepared};
    use gg_compute::graph::task::{
        NamedOptimizerState, aggregate_named, apply_step_named, round_indices, select_rows,
    };
    use gg_compute::graph::templates::{Positions, TransformerDims, transformer};
    use gg_compute::graph::{memory, stages};
    use gg_protocol::tensor_file::{bf16_bits, bf16_value};

    let env = start_with(|c| c.node_relay_max_bytes = 0).await;
    let (token, project) = researcher(&env, "Ada").await;
    let dir = tempfile::tempdir().unwrap();
    let node = start_node(&env, &token, project, &dir.path().join("node")).await;
    let (seq, vocab, total) = (4u64, 12u64, 32u64);
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
    let (batch, tasks, seed, rounds) = (8u32, 2u32, 11u64, 2u32);
    let micro = (batch / tasks) as u64;
    let whole = memory::task_memory(&model, &report, micro, Default::default()).total();
    // What the node's local API computes from `stage_memory_bytes`.
    let bounds = stages::partition(&model, &report, micro, whole / 3).unwrap();
    assert!(bounds.len() > 3, "{bounds:?}");

    let graph_bytes = serde_json::to_vec(&model).unwrap();
    let graph_ref = ArtifactRef {
        hash: Sha256Digest::of_bytes(&graph_bytes),
        size: graph_bytes.len() as u64,
    };
    let r = reqwest::Client::new()
        .put(format!(
            "{}/v1/researcher/projects/{project}/artifacts/{}?size={}&schema=gg-graph-v1",
            env.base, graph_ref.hash, graph_ref.size
        ))
        .bearer_auth(&token)
        .body(graph_bytes)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let mut d = gg_protocol::drbg::Drbg::new("node/stages", 1);
    let mut ids = || {
        TensorF32::new(
            vec![total, seq],
            (0..total * seq)
                .map(|_| d.next_index(vocab as u32) as f32)
                .collect(),
        )
        .unwrap()
    };
    let (x, y) = (ids(), ids());
    let mut refs = BTreeMap::new();
    for (name, t) in [("ids", &x), ("y", &y)] {
        let path = dir.path().join(name);
        std::fs::write(&path, t.encode()).unwrap();
        refs.insert(name, node.hold(project, &path, "ggt1-f32").await.unwrap());
    }
    let (s, run) = env
        .post(
            &token,
            &format!("/researcher/projects/{project}/collective"),
            json!({
                "name": "stages", "graph": graph_ref, "datasets": {"ids": refs["ids"], "y": refs["y"]},
                "optimizer": "adam", "learning_rate": 0.01, "batch_per_round": batch,
                "tasks_per_round": tasks, "rounds": rounds, "seed": seed, "max_round_retries": 1,
                "stage_bounds": bounds, "holder_node": node.id(),
            }),
        )
        .await;
    assert_eq!(s, 200, "{run}");
    assert_eq!(run["config"]["graph"]["batch_inputs"], json!(true));
    let run_id = run["run_id"].as_str().unwrap().to_owned();

    let holders = vec![holder(&env).await, holder(&env).await];
    let v = gg_verifier::Verifier::new(gg_verifier::VerifierConfig {
        server_url: env.base.clone(),
        key_path: dir.path().join("validator.key"),
        work_dir: dir.path().join("validator"),
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
    let validating = validate_in_background(v);
    let started = Instant::now();
    let mut steps = 0;
    let view = loop {
        if started.elapsed() > Duration::from_secs(90) {
            let (_, view) = env
                .get(&token, &format!("/researcher/collective/{run_id}"))
                .await;
            let mut jobs = Vec::new();
            for r in view["rounds"].as_array().unwrap() {
                let (_, j) = env
                    .get(
                        &token,
                        &format!("/researcher/jobs/{}", r["job_id"].as_str().unwrap()),
                    )
                    .await;
                jobs.push(j);
            }
            panic!("the run did not finish after {steps} steps: {view}\njobs: {jobs:?}");
        }
        steps += node.step_once().await.unwrap();
        let mut worked = false;
        for h in &holders {
            worked |= work_once(&env, h).await;
        }
        node.pull_once().await.unwrap();
        if worked {
            continue;
        }
        // Idle: see where the run is (not on every pass, the researcher's requests are counted).
        let (s, view) = env
            .get(&token, &format!("/researcher/collective/{run_id}"))
            .await;
        assert_eq!(s, 200, "{view}");
        if matches!(
            view["state"].as_str(),
            Some("completed" | "failed" | "cancelled")
        ) {
            break view;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(view["state"], "completed", "{view}");
    assert_eq!(steps, 1 + rounds as usize);
    let k = bounds.len() - 1;
    let v = validating.finish().await;
    assert!(
        v.decided
            .iter()
            .all(|d| d.outcome == VerificationOutcome::Accepted)
    );
    assert_eq!(v.decided.len(), rounds as usize * 2 * k * tasks as usize);

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
                for (n, mut g) in b.gradients {
                    // Sent in bf16, as the workers send them.
                    g.data
                        .iter_mut()
                        .for_each(|v| *v = bf16_value(bf16_bits(*v)));
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
    // The checkpoint, one shard per stage, on the node.
    let mut published = BTreeMap::new();
    for (_, r) in view["checkpoint"]["weights"].as_object().unwrap() {
        let r: ArtifactRef = serde_json::from_value(r.clone()).unwrap();
        assert!(node.holds(&r.hash).await);
        published.extend(
            BundleF32::decode(
                &std::fs::read(node.path_of(&r.hash)).unwrap(),
                &Default::default(),
            )
            .unwrap()
            .entries,
        );
    }
    assert_eq!(published.len(), weights.len());
    for (name, t) in &weights {
        assert!(
            published[name].data == t.data,
            "{name}: the node's staged checkpoint differs from the replay"
        );
    }
    for h in holders {
        h.server.stop().await;
    }
    node.stop().await;
    env.stop().await;
}

/// The installed node (`gg-node run`): it finds its server in its configuration (pinning the
/// server's key), and the whole training goes through its local API, as the Python library does it
/// — datasets sent to the node, run created by the node, progress read from it, trained weights
/// taken from it.
#[tokio::test]
async fn the_installed_node_trains_through_its_local_api() {
    let env = start_with(|c| c.node_relay_max_bytes = 0).await;
    let (token, project) = researcher(&env, "Ada").await;
    let dir = tempfile::tempdir().unwrap();
    let api = gg_client::client::Api::new_allowing_plain_http(&env.base).unwrap();
    let info = api.info().await.unwrap();

    // The key the administrator created with `gg-node init`, registered and attached.
    let key_path = dir.path().join("node.key");
    let key = gg_protocol::keys::SecretKey::load_or_generate(&key_path).unwrap();
    let (s, n) = env
        .post(
            &token,
            "/researcher/nodes",
            json!({ "public_key": key.public_key().to_hex(), "name": "lab" }),
        )
        .await;
    assert_eq!(s, 200, "{n}");
    let (s, _) = env
        .post(
            &token,
            &format!("/researcher/projects/{project}/nodes"),
            json!({ "node_id": n["node_id"] }),
        )
        .await;
    assert_eq!(s, 200);
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let local = format!("http://127.0.0.1:{port}");
    let entry = |server_key: String| gg_node::directory::ServerEntry {
        name: "test".into(),
        url: env.base.clone(),
        network: info.network_id.to_string(),
        server_public_key: server_key,
    };
    let cfg = gg_node::config::ServiceConfig {
        key_path: key_path.clone(),
        state_dir: dir.path().join("state"),
        servers: vec![entry(info.server_public_key.to_hex())],
        allow_plain_http: true,
        api_listen: format!("127.0.0.1:{port}"),
        p2p_listen: "127.0.0.1:0".into(),
        allow_private_peers: true,
        ..Default::default()
    };
    cfg.validate().unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let service = tokio::spawn(gg_node::service::run(cfg, rx));

    let http = reqwest::Client::new();
    let get = |path: String, bearer: Option<String>| {
        let http = http.clone();
        let local = local.clone();
        async move {
            let mut r = http.get(format!("{local}{path}"));
            if let Some(t) = bearer {
                r = r.bearer_auth(t);
            }
            let r = r.send().await.unwrap();
            let s = r.status().as_u16();
            (s, r.json::<serde_json::Value>().await.unwrap_or_default())
        }
    };
    // Up, and working with the server, once it answers.
    let started = Instant::now();
    loop {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the service did not start"
        );
        if let Ok(r) = http
            .get(format!("{local}/v1/status"))
            .bearer_auth(&token)
            .send()
            .await
            && r.status() == 200
        {
            let v: serde_json::Value = r.json().await.unwrap();
            if v["servers"].as_array().is_some_and(|s| !s.is_empty()) {
                assert_eq!(v["servers"][0]["projects"], json!([project]));
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // No token, or a token the server does not know: refused.
    assert_eq!(get("/v1/status".into(), None).await.0, 401);
    assert_eq!(
        get("/v1/status".into(), Some("gg_nonsense".into())).await.0,
        401
    );

    // Datasets go to the node; the description (small) to the server.
    let (x, y) = dataset();
    let mut refs = BTreeMap::new();
    for (name, t) in [("x", &x), ("y", &y)] {
        let r = http
            .put(format!(
                "{local}/v1/projects/{project}/data?schema=ggt1-f32"
            ))
            .bearer_auth(&token)
            .body(t.encode())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: ArtifactRef = r.json().await.unwrap();
        assert_eq!(v.hash, Sha256Digest::of_bytes(&t.encode()));
        refs.insert(name, v);
    }
    // Another project's data: refused (the control plane says who is a member).
    let (_bob, bob_project) = researcher(&env, "Bob").await;
    let r = http
        .put(format!("{local}/v1/projects/{bob_project}/data"))
        .bearer_auth(&token)
        .body(vec![0u8; 10])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let r = reqwest::Client::new()
        .put(format!(
            "{}/v1/researcher/projects/{project}/artifacts/{}?size={}&schema=gg-graph-v1",
            env.base,
            Sha256Digest::of_bytes(GRAPH.as_bytes()),
            GRAPH.len()
        ))
        .bearer_auth(&token)
        .body(GRAPH.as_bytes().to_vec())
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    // The cutting by the node: asked to tile and to cut into stages, the node does both and sends
    // the server the tiled description, the tiling, its source and the stage bounds.
    let r = http
        .post(format!("{local}/v1/projects/{project}/runs"))
        .bearer_auth(&token)
        .json(&json!({
            "name": "cut", "graph": { "hash": Sha256Digest::of_bytes(GRAPH.as_bytes()), "size": GRAPH.len() },
            "datasets": { "x": refs["x"], "y": refs["y"] },
            "optimizer": "adam", "learning_rate": 0.02, "batch_per_round": 48, "tasks_per_round": 3,
            "rounds": 1, "seed": 5, "tile_max_param_bytes": 128, "stage_memory_bytes": 1u64 << 40,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let cut: serde_json::Value = r.json().await.unwrap();
    let g = &cut["config"]["graph"];
    assert_eq!(g["tiling"].as_array().map(Vec::len), Some(1), "{g}");
    assert_eq!(
        g["source_graph"]["hash"],
        json!(Sha256Digest::of_bytes(GRAPH.as_bytes()))
    );
    assert_ne!(g["graph"]["hash"], g["source_graph"]["hash"]);
    assert!(
        g["stage_bounds"].as_array().is_some_and(|b| b.len() >= 2),
        "{g}"
    );
    let r = http
        .post(format!(
            "{local}/v1/runs/{}/cancel",
            cut["run_id"].as_str().unwrap()
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let r = http
        .post(format!("{local}/v1/projects/{project}/runs"))
        .bearer_auth(&token)
        .json(&json!({
            "name": "local-api", "graph": { "hash": Sha256Digest::of_bytes(GRAPH.as_bytes()), "size": GRAPH.len() },
            "datasets": { "x": refs["x"], "y": refs["y"] },
            "optimizer": "adam", "learning_rate": 0.02, "batch_per_round": 48, "tasks_per_round": 3,
            "rounds": 3, "seed": 5, "max_round_retries": 1,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let run: serde_json::Value = r.json().await.unwrap();
    let run_id = run["run_id"].as_str().unwrap().to_owned();

    let holders = vec![holder(&env).await, holder(&env).await];
    let v = gg_verifier::Verifier::new(gg_verifier::VerifierConfig {
        server_url: env.base.clone(),
        key_path: dir.path().join("validator.key"),
        work_dir: dir.path().join("validator"),
        claim_wait_secs: 2,
        allow_private_peers: true,
        ..Default::default()
    })
    .await
    .unwrap();
    let (s, _) = env
        .post(
            &env.admin,
            "/admin/validators",
            json!({ "public_key": v.public_key().to_hex(), "display_name": "v1" }),
        )
        .await;
    assert_eq!(s, 200);
    let validating = validate_in_background(v);

    // Progress is read from the node: the run as the server sees it, and its round's tasks.
    let mut seen_tasks = false;
    let view = loop {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "the run did not finish"
        );
        for h in &holders {
            work_once(&env, h).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (s, view) = get(format!("/v1/runs/{run_id}"), Some(token.clone())).await;
        assert_eq!(s, 200, "{view}");
        seen_tasks |= view["current_round_tasks"]
            .as_object()
            .is_some_and(|t| !t.is_empty());
        if view["run"]["state"] == "completed" {
            break view;
        }
    };
    assert!(seen_tasks, "the node never reported a round's tasks");
    assert_eq!(view["checkpoint_on_this_node"], json!(true));

    // The trained weights come from the node, and equal the replay bit for bit.
    let r = http
        .get(format!("{local}/v1/runs/{run_id}/checkpoint/checkpoint"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let announced = r.headers()["x-gg-sha256"].to_str().unwrap().to_owned();
    let bytes = r.bytes().await.unwrap();
    assert_eq!(Sha256Digest::of_bytes(&bytes).to_hex(), announced);
    let published = BundleF32::decode(&bytes, &Default::default())
        .unwrap()
        .entries;
    for (name, t) in &replay(&x, &y, 3) {
        assert!(
            published[name].data == t.data,
            "{name} differs from the replay"
        );
    }
    // Someone else's token cannot read it.
    let (bob_token, _) = researcher(&env, "Carol").await;
    let (s, _) = get(
        format!("/v1/runs/{run_id}/checkpoint/checkpoint"),
        Some(bob_token),
    )
    .await;
    assert_eq!(s, 404);

    validating.finish().await;
    let _ = stop.send(true);
    service.await.unwrap().unwrap();
    for h in holders {
        h.server.stop().await;
    }
    env.stop().await;
}

/// A server that does not present the key the list names is not worked with.
#[tokio::test]
async fn the_node_refuses_a_server_with_another_key() {
    let env = start().await;
    let dir = tempfile::tempdir().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let (token, _) = researcher(&env, "Ada").await;
    let impostor = gg_protocol::keys::SecretKey::generate()
        .unwrap()
        .public_key();
    let cfg = gg_node::config::ServiceConfig {
        key_path: dir.path().join("node.key"),
        state_dir: dir.path().join("state"),
        servers: vec![gg_node::directory::ServerEntry {
            name: "test".into(),
            url: env.base.clone(),
            network: "gg-e2e".into(),
            server_public_key: impostor.to_hex(),
        }],
        allow_plain_http: true,
        api_listen: format!("127.0.0.1:{port}"),
        ..Default::default()
    };
    let (stop, rx) = tokio::sync::watch::channel(false);
    let service = tokio::spawn(gg_node::service::run(cfg, rx));
    tokio::time::sleep(Duration::from_millis(800)).await;
    // No server to work with: the API says so rather than acting.
    let r = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/v1/status"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503, "{}", r.text().await.unwrap());
    let _ = stop.send(true);
    service.await.unwrap().unwrap();
    env.stop().await;
}

/// A run a node steps may start from weights the scientist supplies (fine-tuning): the node holds
/// the bundle, checks it against the description, and the training continues from it.
#[tokio::test]
async fn a_node_run_starts_from_the_weights_supplied() {
    supplied_weights(false).await;
}

/// The same with a matrix tiled by the node (the cutting by the node): the node tiled the
/// description, the server checked the tiling against the source, and the node cut the weights
/// supplied for the source into the tiles. Each tile equals the replay of the tiled description.
#[tokio::test]
async fn a_lab_node_tiles_a_matrix_and_the_weights_supplied() {
    supplied_weights(true).await;
}

async fn supplied_weights(tiled: bool) {
    let env = start_with(|c| c.node_relay_max_bytes = 0).await;
    let (token, project) = researcher(&env, "Ada").await;
    let dir = tempfile::tempdir().unwrap();
    let node = start_node(&env, &token, project, &dir.path().join("node")).await;
    let r = reqwest::Client::new()
        .put(format!(
            "{}/v1/researcher/projects/{project}/artifacts/{}?size={}&schema=gg-graph-v1",
            env.base,
            Sha256Digest::of_bytes(GRAPH.as_bytes()),
            GRAPH.len()
        ))
        .bearer_auth(&token)
        .body(GRAPH.as_bytes().to_vec())
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let graph: gg_compute::graph::ModelGraphV1 = serde_json::from_str(GRAPH).unwrap();
    // Weights that are not the seeded ones: those of another seed.
    let supplied = gg_compute::graph::exec::init_parameters(&graph, 77);
    let bundle = BundleF32::new(supplied.clone()).unwrap().encode();
    let path = dir.path().join("start.ggb");
    std::fs::write(&path, &bundle).unwrap();
    let start_ref = node.hold(project, &path, "ggb1-f32").await.unwrap();
    let (x, y) = dataset();
    let mut refs = BTreeMap::new();
    for (name, t) in [("x", &x), ("y", &y)] {
        let p = dir.path().join(name);
        std::fs::write(&p, t.encode()).unwrap();
        refs.insert(name, node.hold(project, &p, "ggt1-f32").await.unwrap());
    }
    let source = json!({ "hash": Sha256Digest::of_bytes(GRAPH.as_bytes()), "size": GRAPH.len() });
    let mut req = json!({
        "name": "fine-tune", "graph": source,
        "datasets": { "x": refs["x"], "y": refs["y"] },
        "optimizer": "adam", "learning_rate": 0.02, "batch_per_round": 48, "tasks_per_round": 3,
        "rounds": 2, "seed": 5, "max_round_retries": 1, "holder_node": node.id(),
        "initial_checkpoint": start_ref,
    });
    // The description the run trains: tiled by the node when asked (w1, 240 bytes, in two tiles).
    let (trained, tiling) = if tiled {
        let (bytes, tiling) = gg_node::api::tile_description(GRAPH.as_bytes(), 128).unwrap();
        assert_eq!(tiling.len(), 1, "{tiling:?}");
        let r = reqwest::Client::new()
            .put(format!(
                "{}/v1/researcher/projects/{project}/artifacts/{}?size={}&schema=gg-graph-v1",
                env.base,
                Sha256Digest::of_bytes(&bytes),
                bytes.len()
            ))
            .bearer_auth(&token)
            .body(bytes.clone())
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
        // The server does not tile a node's model.
        let mut asked = req.clone();
        asked["tile_max_param_bytes"] = json!(128);
        let (s, e) = env
            .post(
                &token,
                &format!("/researcher/projects/{project}/collective"),
                asked,
            )
            .await;
        assert_eq!(s, 422, "{e}");
        req["graph"] = json!({ "hash": Sha256Digest::of_bytes(&bytes), "size": bytes.len() });
        req["source_graph"] = source.clone();
        // A tiling that does not give the description is refused.
        let mut lie = req.clone();
        let mut other = serde_json::to_value(&tiling).unwrap();
        other[0]["tiles"] = json!(3);
        lie["tiling"] = other;
        let (s, e) = env
            .post(
                &token,
                &format!("/researcher/projects/{project}/collective"),
                lie,
            )
            .await;
        assert_eq!(s, 422, "{e}");
        req["tiling"] = serde_json::to_value(&tiling).unwrap();
        (serde_json::from_slice(&bytes).unwrap(), tiling)
    } else {
        (graph.clone(), Vec::new())
    };
    // A bundle of the wrong size, or one the node does not hold, is refused.
    let mut wrong = req.clone();
    wrong["initial_checkpoint"] = json!({ "hash": start_ref.hash, "size": start_ref.size + 4 });
    let (s, e) = env
        .post(
            &token,
            &format!("/researcher/projects/{project}/collective"),
            wrong,
        )
        .await;
    assert_eq!(s, 422, "{e}");
    let (s, run) = env
        .post(
            &token,
            &format!("/researcher/projects/{project}/collective"),
            req.clone(),
        )
        .await;
    assert_eq!(s, 200, "{run}");
    let run_id = run["run_id"].as_str().unwrap().to_owned();
    req["name"] = json!("unused");

    let holders = vec![holder(&env).await, holder(&env).await];
    let v = gg_verifier::Verifier::new(gg_verifier::VerifierConfig {
        server_url: env.base.clone(),
        key_path: dir.path().join("validator.key"),
        work_dir: dir.path().join("validator"),
        claim_wait_secs: 2,
        allow_private_peers: true,
        ..Default::default()
    })
    .await
    .unwrap();
    let (s, _) = env
        .post(
            &env.admin,
            "/admin/validators",
            json!({ "public_key": v.public_key().to_hex(), "display_name": "v1" }),
        )
        .await;
    assert_eq!(s, 200);
    let validating = validate_in_background(v);
    let started = Instant::now();
    let mut first = true;
    let view = loop {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "the run did not finish"
        );
        node.step_once().await.unwrap();
        if first && !tiled {
            // Step 0 is the bundle supplied, as it is.
            let (_, view) = env
                .get(&token, &format!("/researcher/collective/{run_id}"))
                .await;
            assert_eq!(
                view["checkpoint"]["weights"]["checkpoint"]["hash"],
                json!(start_ref.hash)
            );
        }
        first = false;
        for h in &holders {
            work_once(&env, h).await;
        }
        node.pull_once().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (_, view) = env
            .get(&token, &format!("/researcher/collective/{run_id}"))
            .await;
        if view["state"] == "completed" {
            break view;
        }
    };
    let ck: ArtifactRef =
        serde_json::from_value(view["checkpoint"]["weights"]["checkpoint"].clone()).unwrap();
    let published = BundleF32::decode(
        &std::fs::read(node.path_of(&ck.hash)).unwrap(),
        &Default::default(),
    )
    .unwrap()
    .entries;
    let start = gg_compute::graph::tiling::split_weights(&supplied, &tiling).unwrap();
    let reference = replay_graph(&trained, &x, &y, 2, start);
    assert_eq!(published.len(), reference.len());
    for (name, t) in &reference {
        assert!(
            published[name].data == t.data,
            "{name} differs from the replay from the supplied weights"
        );
    }
    if tiled {
        assert!(published.contains_key(&gg_compute::graph::tiling::tile_name("w1", 0, 2)));
        assert_eq!(
            view["config"]["graph"]["tiling"],
            serde_json::to_value(&tiling).unwrap()
        );
    }
    validating.finish().await;
    for h in holders {
        h.server.stop().await;
    }
    node.stop().await;
    env.stop().await;
}
