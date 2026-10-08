//! A lab node nobody can reach (ADR-0041, A8): it still holds its artifacts, and the control plane
//! relays the small ones, chunk by chunk, from the node to the readers; nothing is stored on the
//! server, every chunk is checked by the reader against the signed manifest, and an artifact above
//! the relay limit is not relayed.

mod common;

use std::time::Duration;

use common::*;
use gg_protocol::tensor_file::TensorF32;
use serde_json::json;

#[tokio::test]
async fn an_unreachable_node_serves_small_artifacts_through_the_server() {
    // The server refuses loopback addresses: nothing the node announces is accepted, as for a node
    // behind a NAT it cannot open.
    let env = start_with(|c| {
        c.p2p_allow_private_addrs = false;
        c.node_relay_max_bytes = 200 * 1024;
        c.node_relay_wait_secs = 2;
    })
    .await;
    let (token, project) = researcher(&env, "Ada").await;
    let dir = tempfile::tempdir().unwrap();
    let node = start_node(&env, &token, project, &dir.path().join("node")).await;
    let (stop, rx) = tokio::sync::watch::channel(false);
    let relaying = tokio::spawn(node.clone().keep_relaying(Duration::from_secs(1), rx));

    // 96 KiB and 48 KiB inputs, below the relay limit; a 256 KiB one above it.
    let mut d = gg_protocol::drbg::Drbg::new("relay", 1);
    let a = TensorF32::new(vec![128, 192], d.sym_f32_vec(128 * 192)).unwrap();
    let b = TensorF32::new(vec![192, 64], d.sym_f32_vec(192 * 64)).unwrap();
    let big = TensorF32::new(vec![256, 256], d.sym_f32_vec(256 * 256)).unwrap();
    let mut refs = Vec::new();
    for (i, t) in [&a, &b, &big].into_iter().enumerate() {
        let path = dir.path().join(format!("in{i}"));
        std::fs::write(&path, t.encode()).unwrap();
        refs.push(node.hold(project, &path, "ggt1-f32").await.unwrap());
    }
    // Held, but announced nowhere.
    assert_eq!(node.reachable_at().await, None);

    let def = gg_compute::catalog::matmul_definition(128, 192, 64, refs[0], refs[1]).unwrap();
    let def_big = gg_compute::catalog::matmul_definition(256, 256, 256, refs[2], refs[2]).unwrap();
    let (s, job) = env
        .post(
            &token,
            &format!("/researcher/projects/{project}/jobs"),
            json!({ "name": "mm", "tasks": [def, def_big] }),
        )
        .await;
    assert_eq!(s, 200, "{job}");
    let w = worker(&env).await;
    let mut claimed = Vec::new();
    for _ in 0..2 {
        let (s, c) = env
            .post(
                &w.token,
                "/worker/claim",
                json!({ "kinds": ["tensor.matmul.v1"], "wait_secs": 1 }),
            )
            .await;
        assert_eq!(s, 200, "{c}");
        claimed.push(c);
    }

    let api = gg_client::client::Api::new_allowing_plain_http(&env.base).unwrap();
    let info = api.info().await.unwrap();
    // Through the peer path: no peer, the server says it serves, and each chunk comes relayed.
    let (s, peers) = env
        .get(
            &w.token,
            &format!("/worker/artifacts/{}/peers", refs[0].hash),
        )
        .await;
    assert_eq!(s, 200, "{peers}");
    assert_eq!(peers["peers"], json!([]));
    assert_eq!(peers["server_holds"], json!(true));
    let dest = dir.path().join("a");
    let stats = gg_client::p2p::fetch_with_peers(
        &api,
        &w.token,
        &w.key,
        &info.network_id,
        &info.server_public_key,
        &refs[0],
        &format!("/worker/artifacts/{}", refs[0].hash),
        &dest,
        &gg_client::p2p::FetchConfig::default(),
    )
    .await
    .unwrap();
    assert_eq!(stats.chunks_from_http, 2);
    assert_eq!(std::fs::read(&dest).unwrap(), a.encode());
    // A plain download of the whole artifact, relayed too, and checked against its hash.
    let dest = dir.path().join("b");
    api.download(
        &w.token,
        &format!("/worker/artifacts/{}", refs[1].hash),
        &refs[1],
        1 << 20,
        &dest,
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), b.encode());
    // The server keeps nothing of what it relayed.
    let (s, _) = env
        .get(
            &token,
            &format!("/researcher/projects/{project}/artifacts/{}", refs[1].hash),
        )
        .await;
    assert_ne!(s, 200);
    // Above the relay limit: not relayed.
    let (s, body) = env
        .get(&w.token, &format!("/worker/artifacts/{}", refs[2].hash))
        .await;
    assert_eq!(s, 409, "{body}");

    // The node stops answering: the reader is told to retry (503), not handed broken bytes.
    let _ = stop.send(true);
    relaying.await.unwrap();
    let r = reqwest::Client::new()
        .get(format!("{}/v1/worker/artifacts/{}", env.base, refs[1].hash))
        .bearer_auth(&w.token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 503);

    node.stop().await;
    env.stop().await;
}
