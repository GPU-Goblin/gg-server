//! A lab node holds a run's inputs (ADR-0041, A3), against a real control plane and PostgreSQL:
//! the server records hash, size and the node's chunk digests and keeps no copy; a worker assigned
//! a task over them gets a signed manifest and a ticket and takes every chunk from the node over
//! pinned TLS; the server refuses to serve them itself; only the node that holds an artifact may
//! announce it; a revoked node is no longer advertised.

mod common;

use common::*;
use gg_core::*;
use gg_protocol::keys::SecretKey;
use gg_protocol::messages::ArtifactRef;
use gg_protocol::tensor_file::TensorF32;
use serde_json::json;

#[tokio::test]
async fn a_worker_takes_a_node_held_input_from_the_node_only() {
    // Nothing relayed by the server here (A8 tests that): the inputs come from the node only.
    let env = start_with(|c| c.node_relay_max_bytes = 0).await;
    let (token, project) = researcher(&env, "Ada").await;
    let dir = tempfile::tempdir().unwrap();
    let node = start_node(&env, &token, project, &dir.path().join("node")).await;

    // 96 KiB and 48 KiB: two chunks of 64 KiB, and one.
    let mut d = gg_protocol::drbg::Drbg::new("node", 1);
    let a = TensorF32::new(vec![128, 192], d.sym_f32_vec(128 * 192)).unwrap();
    let b = TensorF32::new(vec![192, 64], d.sym_f32_vec(192 * 64)).unwrap();
    let mut refs: Vec<ArtifactRef> = Vec::new();
    let mut files = Vec::new();
    for (i, t) in [&a, &b].into_iter().enumerate() {
        let path = dir.path().join(format!("in{i}"));
        std::fs::write(&path, t.encode()).unwrap();
        let r = node.hold(project, &path, "ggt1-f32").await.unwrap();
        assert_eq!(r.hash, Sha256Digest::of_bytes(&t.encode()));
        refs.push(r);
        files.push(t.encode());
    }
    // The scientist may change the file afterwards: the node serves its own copy.
    std::fs::write(dir.path().join("in0"), b"changed").unwrap();
    // Declared twice (a restart): the same answer.
    let again = node
        .hold(project, &dir.path().join("in1"), "ggt1-f32")
        .await
        .unwrap();
    assert_eq!(again, refs[1]);

    // A job over them is accepted although the server stores no copy.
    let def = gg_compute::catalog::matmul_definition(128, 192, 64, refs[0], refs[1]).unwrap();
    let (s, job) = env
        .post(
            &token,
            &format!("/researcher/projects/{project}/jobs"),
            json!({ "name": "mm", "tasks": [def] }),
        )
        .await;
    assert_eq!(s, 200, "{job}");

    let w = worker(&env).await;
    let (s, claim) = env
        .post(
            &w.token,
            "/worker/claim",
            json!({ "kinds": ["tensor.matmul.v1"], "wait_secs": 2 }),
        )
        .await;
    assert_eq!(s, 200, "{claim}");

    // The server does not serve it: the node does.
    let (s, body) = env
        .get(&w.token, &format!("/worker/artifacts/{}", refs[0].hash))
        .await;
    assert_eq!(s, 409, "{body}");
    assert!(body.to_string().contains("lab node"), "{body}");
    let (s, peers) = env
        .get(
            &w.token,
            &format!("/worker/artifacts/{}/peers", refs[0].hash),
        )
        .await;
    assert_eq!(s, 200, "{peers}");
    assert_eq!(peers["server_holds"], json!(false));
    assert_eq!(peers["peers"][0]["node_id"], json!(node.id().to_string()));
    // The node's chunk server listens on loopback: the address it announced is the bound one.
    let (addr, how) = node
        .reachable_at()
        .await
        .expect("the node announced an address");
    assert_eq!(how, "bound");
    assert_eq!(peers["peers"][0]["addr"], json!(addr));

    let api = gg_client::client::Api::new_allowing_plain_http(&env.base).unwrap();
    let info = api.info().await.unwrap();
    for (r, bytes) in refs.iter().zip(&files) {
        let dest = dir.path().join(format!("got-{}", r.hash));
        let stats = gg_client::p2p::fetch_with_peers(
            &api,
            &w.token,
            &w.key,
            &info.network_id,
            &info.server_public_key,
            r,
            &format!("/worker/artifacts/{}", r.hash),
            &dest,
            &gg_client::p2p::FetchConfig {
                allow_private_peers: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(stats.chunks_from_http, 0);
        assert_eq!(stats.chunks_from_peers as u64, r.size.div_ceil(64 * 1024));
        assert_eq!(&std::fs::read(&dest).unwrap(), bytes);
    }
    assert_eq!(node.chunks_served(), 3);

    // Revoked: no longer advertised, and a reader waits for peers rather than the server.
    let (s, list) = env.get(&token, "/researcher/nodes").await;
    assert_eq!(s, 200);
    let node_id = list[0]["node_id"].as_str().unwrap();
    let (s, _) = env
        .post(
            &token,
            &format!("/researcher/nodes/{node_id}/revoke"),
            json!({}),
        )
        .await;
    assert_eq!(s, 200);
    let (s, peers) = env
        .get(
            &w.token,
            &format!("/worker/artifacts/{}/peers", refs[0].hash),
        )
        .await;
    assert_eq!(s, 200, "{peers}");
    assert_eq!(peers["peers"], json!([]));
    let started = std::time::Instant::now();
    let err = gg_client::p2p::fetch_with_peers(
        &api,
        &w.token,
        &w.key,
        &info.network_id,
        &info.server_public_key,
        &refs[1],
        &format!("/worker/artifacts/{}", refs[1].hash),
        &dir.path().join("lost"),
        &gg_client::p2p::FetchConfig {
            allow_private_peers: true,
            request_timeout_secs: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            gg_client::client::ClientError::Status { status: 409, .. }
        ),
        "{err}"
    );
    assert!(started.elapsed() >= std::time::Duration::from_secs(1));

    node.stop().await;
    env.stop().await;
}

#[tokio::test]
async fn only_the_holding_node_declares_and_announces_an_artifact() {
    let env = start().await;
    let (ada, project) = researcher(&env, "Ada").await;
    let (bob, bob_project) = researcher(&env, "Bob").await;
    let dir = tempfile::tempdir().unwrap();
    let node = start_node(&env, &ada, project, &dir.path().join("ada")).await;
    let other = start_node(&env, &bob, bob_project, &dir.path().join("bob")).await;
    let path = dir.path().join("data");
    std::fs::write(&path, vec![7u8; 100_000]).unwrap();

    // Not a member of the project: it does not exist for this node.
    let e = other.hold(project, &path, "").await.unwrap_err();
    assert!(e.to_string().contains("404"), "{e}");
    let r = node.hold(project, &path, "").await.unwrap();
    // The same hash declared by another node: refused.
    let e = other.hold(bob_project, &path, "").await.unwrap_err();
    assert!(e.to_string().contains("409"), "{e}");

    // Announcing what another node holds: not found. Wrong chunk counts: refused.
    let api = gg_client::client::Api::new_allowing_plain_http(&env.base).unwrap();
    let info = api.info().await.unwrap();
    let key = SecretKey::generate().unwrap();
    let (s, n) = env
        .post(
            &bob,
            "/researcher/nodes",
            json!({ "public_key": key.public_key().to_hex() }),
        )
        .await;
    assert_eq!(s, 200);
    let (s, _) = env
        .post(
            &bob,
            &format!("/researcher/projects/{bob_project}/nodes"),
            json!({ "node_id": n["node_id"] }),
        )
        .await;
    assert_eq!(s, 200);
    let t = api
        .login(&key, &info.network_id, "node-login")
        .await
        .unwrap()
        .token;
    let (s, body) = env
        .post(
            &t,
            &format!("/node/artifacts/{}/announce", r.hash),
            json!({ "addr": "127.0.0.1:9", "tls_fingerprint": Sha256Digest::of_bytes(b"k") }),
        )
        .await;
    assert_eq!(s, 404, "{body}");
    let (s, body) = env
        .post(
            &t,
            &format!("/node/artifacts/{}", Sha256Digest::of_bytes(b"x")),
            json!({ "project_id": bob_project, "size": 100_000, "chunks": [Sha256Digest::of_bytes(b"x")] }),
        )
        .await;
    assert_eq!(s, 422, "{body}");
    // A researcher token is not a node session.
    let (s, _) = env
        .post(
            &bob,
            &format!("/node/artifacts/{}", Sha256Digest::of_bytes(b"x")),
            json!({ "project_id": bob_project, "size": 1, "chunks": [Sha256Digest::of_bytes(b"x")] }),
        )
        .await;
    assert!(s == 401 || s == 403, "{s}");

    node.stop().await;
    other.stop().await;
    env.stop().await;
}

/// Many readers asking for the peers of an artifact at once: each is answered, quickly. The peer
/// list once held a database connection while it took a second one (to decide on the relay), so
/// as many requests as the pool has connections deadlocked it until they timed out.
#[tokio::test]
async fn many_readers_at_once_do_not_exhaust_the_database_pool() {
    let env = start().await;
    let (token, project) = researcher(&env, "Ada").await;
    let dir = tempfile::tempdir().unwrap();
    let key = gg_protocol::keys::SecretKey::generate().unwrap();
    let node = start_node_keyed(
        &env,
        &token,
        project,
        &dir.path().join("node"),
        std::time::Duration::from_secs(30),
        gg_protocol::keys::SecretKey::from_bytes(&key.to_bytes()),
    )
    .await;
    let path = dir.path().join("data");
    std::fs::write(&path, vec![5u8; 100_000]).unwrap();
    let r = node.hold(project, &path, "").await.unwrap();
    let api = gg_client::client::Api::new_allowing_plain_http(&env.base).unwrap();
    let info = api.info().await.unwrap();
    let nt = api
        .login(&key, &info.network_id, "node-login")
        .await
        .unwrap()
        .token;
    let started = std::time::Instant::now();
    // Five times the test pool's connections.
    let path = format!("/node/artifacts/{}/peers", r.hash);
    let asked = futures_util::future::join_all((0..40).map(|_| env.get(&nt, &path))).await;
    for (s, body) in &asked {
        assert_eq!(*s, 200, "{body}");
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    node.stop().await;
    env.stop().await;
}
