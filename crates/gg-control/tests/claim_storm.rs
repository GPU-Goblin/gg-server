//! Claim storm: many workers claim concurrently; every task must be assigned exactly once per
//! generation and the scheduler must not stall. Measures claim throughput (documented, not asserted
//! beyond a generous bound).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gg_control::config::ServerConfig;
use gg_control::server::serve;
use gg_control::state::AppState;
use gg_core::*;
use gg_protocol::domain::Domain;
use gg_protocol::keys::SecretKey;
use gg_protocol::messages::{ArtifactRef, LoginChallengeV1};
use gg_protocol::signed::Signed;
use gg_protocol::tensor_file::TensorF32;
use gg_storage::queries::identities;
use gg_storage::testing::TestDb;
use serde_json::{Value, json};

async fn login(http: &reqwest::Client, base: &str, key: &SecretKey, net: &NetworkId) -> String {
    let ch: Value = http
        .post(format!("{base}/v1/worker/challenge"))
        .json(&json!({ "public_key": key.public_key().to_hex() }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let nonce = hex::decode(ch["nonce"].as_str().unwrap()).unwrap();
    let body = LoginChallengeV1 {
        protocol_version: gg_protocol::PROTOCOL_VERSION,
        network: net.clone(),
        action: "worker-login".into(),
        nonce,
        public_key: key.public_key(),
        issued_at: UnixMillis::now(),
    };
    let signed = Signed::sign(key, net, Domain::Login, body).unwrap();
    let s: Value = http
        .post(format!("{base}/v1/worker/session"))
        .json(&signed)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    s["token"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn sixty_workers_claim_two_hundred_tasks_exactly_once() {
    let tdb = TestDb::create_or_panic().await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = ServerConfig {
        database_url: tdb.url(),
        data_dir: tmp.path().to_path_buf(),
        network_id: "gg-storm".into(),
        claim_wait_max_secs: 2,
        quota_requests_per_minute: 60_000,
        quota_burst: 10_000,
        pool_size: 32,
        ..ServerConfig::default()
    };
    let net = NetworkId::new("gg-storm").unwrap();
    let state = AppState::new(cfg, tdb.db.clone()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let running = serve(state.clone(), listener).await.unwrap();
    let base = running.url();
    let http = reqwest::Client::builder()
        .pool_max_idle_per_host(64)
        .build()
        .unwrap();

    // Project with budget, one tiny input pair, 200 tasks in one job.
    let mut c = tdb.db.pool().acquire().await.unwrap();
    let admin = gg_control::auth::new_token(gg_control::auth::API_TOKEN_PREFIX).unwrap();
    identities::create_token(
        &mut c,
        &gg_control::auth::token_hash(&admin),
        "admin",
        None,
        "t",
        None,
    )
    .await
    .unwrap();
    drop(c);
    let r: Value = http
        .post(format!("{base}/v1/admin/researchers"))
        .bearer_auth(&admin)
        .json(&json!({ "display_name": "S", "admission": "approved" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rtoken = r["token"].as_str().unwrap().to_owned();
    let rid = r["researcher_id"].as_str().unwrap().to_owned();
    let p: Value = http.post(format!("{base}/v1/admin/projects")).bearer_auth(&admin).json(&json!({ "name": "storm", "allowed_kinds": ["tensor.matmul.v1"], "owner_researcher_id": rid, "data_policy_ack": true })).send().await.unwrap().json().await.unwrap();
    let project = p["project_id"].as_str().unwrap().to_owned();
    http.post(format!("{base}/v1/admin/projects/{project}/grant"))
        .bearer_auth(&admin)
        .json(&json!({ "amount": "100000" }))
        .send()
        .await
        .unwrap();
    let t = TensorF32::new(vec![2, 2], vec![1.0, 2.0, 3.0, 4.0]).unwrap();
    let bytes = t.encode();
    let hash = Sha256Digest::of_bytes(&bytes);
    let r = http
        .put(format!(
            "{base}/v1/researcher/projects/{project}/artifacts/{hash}?size={}",
            bytes.len()
        ))
        .bearer_auth(&rtoken)
        .body(bytes.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let aref = ArtifactRef {
        hash,
        size: bytes.len() as u64,
    };
    let def = gg_compute::catalog::matmul_definition(2, 2, 2, aref, aref).unwrap();
    let tasks: Vec<_> = (0..200).map(|_| def.clone()).collect();
    let j: Value = http
        .post(format!("{base}/v1/researcher/projects/{project}/jobs"))
        .bearer_auth(&rtoken)
        .json(&json!({ "name": "storm", "tasks": tasks }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(j["tasks"].as_array().unwrap().len(), 200, "{j}");

    // 60 workers registered and authorized.
    let mut tokens = Vec::new();
    for i in 0..60 {
        let key = SecretKey::generate().unwrap();
        let reg: Value = http.post(format!("{base}/v1/worker/register")).json(&json!({ "public_key": key.public_key().to_hex(), "display_name": format!("w{i}") })).send().await.unwrap().json().await.unwrap();
        let wid = reg["worker_id"].as_str().unwrap();
        http.post(format!("{base}/v1/admin/workers/{wid}/authorize"))
            .bearer_auth(&admin)
            .json(&json!({ "authorized": true }))
            .send()
            .await
            .unwrap();
        tokens.push(login(&http, &base, &key, &net).await);
    }

    // Storm: each worker claims in a loop and abandons immediately (re-queue), until 200 distinct
    // tasks have been seen at generation 1; later generations are counted separately.
    let seen: Arc<tokio::sync::Mutex<HashMap<(String, u64), usize>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let claims = Arc::new(AtomicUsize::new(0));
    let start = std::time::Instant::now();
    let mut handles = Vec::new();
    for token in tokens {
        let http = http.clone();
        let base = base.clone();
        let seen = seen.clone();
        let claims = claims.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..40 {
                let r = http
                    .post(format!("{base}/v1/worker/claim"))
                    .bearer_auth(&token)
                    .json(&json!({ "kinds": ["tensor.matmul.v1"], "wait_secs": 0 }))
                    .send()
                    .await
                    .unwrap();
                if r.status().as_u16() == 204 {
                    let n = seen.lock().await.keys().filter(|(_, g)| *g == 1).count();
                    if n >= 200 {
                        break;
                    }
                    continue;
                }
                assert_eq!(r.status().as_u16(), 200);
                let c: Value = r.json().await.unwrap();
                let task = c["assignment"]["body"]["task_id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let generation = c["assignment"]["body"]["generation"].as_u64().unwrap();
                let attempt = c["assignment"]["body"]["attempt_id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                claims.fetch_add(1, Ordering::SeqCst);
                *seen.lock().await.entry((task, generation)).or_insert(0) += 1;
                let r = http
                    .post(format!("{base}/v1/worker/attempts/{attempt}/abandon"))
                    .bearer_auth(&token)
                    .json(&json!({ "reason": "storm" }))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(r.status().as_u16(), 200);
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let elapsed = start.elapsed();
    let seen = seen.lock().await;
    let duplicates: Vec<_> = seen.iter().filter(|(_, n)| **n > 1).collect();
    assert!(
        duplicates.is_empty(),
        "a (task, generation) was assigned more than once: {duplicates:?}"
    );
    let gen1 = seen.keys().filter(|(_, g)| *g == 1).count();
    assert_eq!(
        gen1, 200,
        "every task must have been assigned at generation 1"
    );
    let total = claims.load(Ordering::SeqCst);
    eprintln!(
        "claim storm: {total} claims by 60 workers in {elapsed:?} ({:.0} claims/s)",
        total as f64 / elapsed.as_secs_f64()
    );
    assert!(elapsed.as_secs() < 60, "storm took too long: {elapsed:?}");
    running.stop().await;
    tdb.drop().await;
}
