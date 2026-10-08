//! The reversed connection (ADR-0041, A8): a reader that cannot reach the holder of an artifact,
//! but can itself be reached, has the holder connect to it and push the chunks. The control plane
//! only puts them in touch: it dials the reader back and signs a push ticket naming the reader's
//! transport key; no byte of the artifact passes through it. The reader takes a chunk only under
//! a ticket naming it, for an artifact it waits for, and matching the signed manifest.

mod common;

use std::time::Duration;

use common::*;
use gg_client::cache::ArtifactCache;
use gg_client::p2p::{PUSH_TICKET_HEADER, PeerServer, PushRequest};
use gg_protocol::keys::SecretKey;
use gg_protocol::tensor_file::TensorF32;
use serde_json::json;

async fn reader(env: &Env, dir: &std::path::Path) -> PeerServer {
    let api = gg_client::client::Api::new_allowing_plain_http(&env.base).unwrap();
    let info = api.info().await.unwrap();
    PeerServer::start(
        "127.0.0.1:0",
        ArtifactCache::new(dir.to_path_buf(), u64::MAX),
        info.network_id,
        info.server_public_key,
        8,
        u64::MAX,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn a_node_nobody_can_reach_pushes_its_input_to_the_worker() {
    let env = start().await;
    let (token, project) = researcher(&env, "Ada").await;
    let dir = tempfile::tempdir().unwrap();
    let key = SecretKey::generate().unwrap();
    let node_key = SecretKey::from_bytes(&key.to_bytes());
    let node = start_node_keyed(
        &env,
        &token,
        project,
        &dir.path().join("node"),
        Duration::from_secs(30),
        node_key,
    )
    .await;

    let mut d = gg_protocol::drbg::Drbg::new("reverse", 1);
    let a = TensorF32::new(vec![128, 192], d.sym_f32_vec(128 * 192)).unwrap();
    let b = TensorF32::new(vec![192, 64], d.sym_f32_vec(192 * 64)).unwrap();
    let mut refs = Vec::new();
    for (i, t) in [&a, &b].into_iter().enumerate() {
        let path = dir.path().join(format!("in{i}"));
        std::fs::write(&path, t.encode()).unwrap();
        refs.push(node.hold(project, &path, "ggt1-f32").await.unwrap());
    }
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
    let (s, c) = env
        .post(
            &w.token,
            "/worker/claim",
            json!({ "kinds": ["tensor.matmul.v1"], "wait_secs": 1 }),
        )
        .await;
    assert_eq!(s, 200, "{c}");
    // (On loopback the node is reachable; the worker takes the reversed path all the same, as
    // gg-worker does when no peer it can reach holds the input.)
    let api = gg_client::client::Api::new_allowing_plain_http(&env.base).unwrap();
    let info = api.info().await.unwrap();
    let node_token = api
        .login(&key, &info.network_id, "node-login")
        .await
        .unwrap()
        .token;
    let listener = reader(&env, &dir.path().join("w")).await;
    let other = reader(&env, &dir.path().join("x")).await;

    // The worker asks to be pushed the first input. An address the control plane cannot dial back
    // is skipped for the next one.
    let dest = dir.path().join("a");
    let fetch = {
        let api = api.clone();
        let w_token = w.token.clone();
        let info = info.clone();
        let artifact = refs[0];
        let dest = dest.clone();
        let addrs = vec!["127.0.0.1:9".to_owned(), listener.addr().to_string()];
        let inbox_server = listener;
        tokio::spawn(async move {
            let r = gg_client::p2p::fetch_by_push(
                &api,
                &w_token,
                "/worker",
                &info.network_id,
                &info.server_public_key,
                &artifact,
                &dest,
                &inbox_server,
                &addrs,
                Duration::from_secs(20),
            )
            .await;
            (r, inbox_server)
        })
    };
    // The request the control plane queued for the node (as its push loop would take it).
    let mut req: Option<PushRequest> = None;
    for _ in 0..20 {
        let asked: Vec<PushRequest> = api
            .get_json(&node_token, "/node/pushes?wait_secs=1")
            .await
            .unwrap();
        if let Some(r) = asked.into_iter().next() {
            req = Some(r);
            break;
        }
    }
    let req = req.expect("the node is asked to push");
    assert_eq!(req.hash, refs[0].hash);
    assert_eq!(req.size, refs[0].size);

    // The ticket names the worker's transport key: another reader refuses it.
    let mut elsewhere = req.clone();
    elsewhere.reader_addr = other.addr().to_string();
    elsewhere.reader_fingerprint = other.fingerprint();
    let cache = ArtifactCache::new(dir.path().join("node").join("store"), u64::MAX);
    let e = gg_client::p2p::push_artifact(&cache, &elsewhere, Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(e.contains("403"), "{e}");
    // A chunk that is not the manifest's is refused by the reader.
    let mut session = gg_client::peer_tls::PeerSession::connect(
        &req.reader_addr,
        req.reader_fingerprint,
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    let (status, _) = session
        .put(
            &format!("/p2p/v1/artifacts/{}/push/0", req.hash),
            &[(PUSH_TICKET_HEADER, req.ticket.clone())],
            vec![0u8; 64 * 1024],
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(status, 422);
    // Without a ticket: refused.
    let (status, _) = session
        .put(
            &format!("/p2p/v1/artifacts/{}/push/0", req.hash),
            &[],
            vec![0u8; 16],
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(status, 403);

    // The node's push loop carries it out: the worker has the input, the server stored nothing.
    let (stop, rx) = tokio::sync::watch::channel(false);
    let pushing = tokio::spawn(node.clone().keep_pushing(Duration::from_secs(1), rx));
    let (s, _) = env
        .post(
            &w.token,
            &format!("/worker/artifacts/{}/reverse", refs[0].hash),
            json!({ "addr": req.reader_addr, "tls_fingerprint": req.reader_fingerprint }),
        )
        .await;
    assert_eq!(s, 200);
    let (r, listener) = fetch.await.unwrap();
    r.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), a.encode());
    let (s, _) = env
        .get(
            &token,
            &format!("/researcher/projects/{project}/artifacts/{}", refs[0].hash),
        )
        .await;
    assert_ne!(s, 200);

    // A reader may only ask for what it may read, at an address the server can dial back.
    let (s, _) = env
        .post(
            &w.token,
            &format!(
                "/worker/artifacts/{}/reverse",
                gg_core::Sha256Digest::of_bytes(b"x")
            ),
            json!({ "addr": listener.addr().to_string(), "tls_fingerprint": listener.fingerprint() }),
        )
        .await;
    assert_eq!(s, 404);
    let (s, _) = env
        .post(
            &w.token,
            &format!("/worker/artifacts/{}/reverse", refs[1].hash),
            json!({ "addr": "127.0.0.1:9", "tls_fingerprint": listener.fingerprint() }),
        )
        .await;
    assert!((400..500).contains(&s), "{s}");

    let _ = stop.send(true);
    pushing.await.unwrap();
    listener.stop().await;
    other.stop().await;
    node.stop().await;
    env.stop().await;
}
