//! Tracker side of peer-assisted distribution (ADR-0012) over HTTP against a real PostgreSQL:
//! only a worker entitled to read an artifact gets its manifest, announces holding it, or receives
//! peers and a ticket; the manifest is signed by the server and describes the server's own bytes;
//! announcements expire and disappear when a worker is deauthorized; the address policy is
//! enforced before another worker is ever told to connect somewhere.

use std::collections::BTreeMap;

use gg_control::config::ServerConfig;
use gg_control::server::{Running, serve};
use gg_control::state::AppState;
use gg_core::*;
use gg_protocol::domain::Domain;
use gg_protocol::keys::{PublicKey, SecretKey};
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
    server_key: PublicKey,
    _tmp: tempfile::TempDir,
}

async fn start(allow_private: bool, peer_ttl_secs: i64) -> Env {
    start_with(allow_private, peer_ttl_secs, false).await
}

/// `dial_back` makes the control plane check that an announced address accepts a connection before
/// advertising it (ADR-0013). Most tests announce addresses nobody listens on, so they turn it off
/// and one dedicated test exercises it.
async fn start_with(allow_private: bool, peer_ttl_secs: i64, dial_back: bool) -> Env {
    let tdb = TestDb::create_or_panic().await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = ServerConfig {
        database_url: tdb.url(),
        data_dir: tmp.path().to_path_buf(),
        network_id: "gg-e2e".into(),
        lease_secs: 120,
        lease_max_secs: 3600,
        reaper_interval_secs: 1,
        claim_wait_max_secs: 5,
        p2p_allow_private_addrs: allow_private,
        p2p_peer_ttl_secs: peer_ttl_secs,
        p2p_chunk_bytes: 64 * 1024,
        p2p_dial_back: dial_back,
        ..ServerConfig::default()
    };
    let state = AppState::new(cfg, tdb.db.clone()).await.unwrap();
    let server_key = state.server_key().public_key();
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
        server_key,
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
    async fn login(&self, key: &SecretKey) -> String {
        let (s, ch) = self
            .post(
                "",
                "/worker/challenge",
                json!({ "public_key": key.public_key().to_hex() }),
            )
            .await;
        assert_eq!(s, 200, "{ch}");
        let nonce = hex::decode(ch["nonce"].as_str().unwrap()).unwrap();
        let body = LoginChallengeV1 {
            protocol_version: gg_protocol::PROTOCOL_VERSION,
            network: self.network.clone(),
            action: "worker-login".into(),
            nonce,
            public_key: key.public_key(),
            issued_at: UnixMillis::now(),
        };
        let signed = Signed::sign(key, &self.network, Domain::Login, body).unwrap();
        let (s, sess) = self
            .post(
                "",
                "/worker/session",
                serde_json::to_value(&signed).unwrap(),
            )
            .await;
        assert_eq!(s, 200, "{sess}");
        sess["token"].as_str().unwrap().to_owned()
    }
}

struct Worker {
    key: SecretKey,
    token: String,
    id: WorkerId,
}

async fn setup_worker(env: &Env, name: &str) -> Worker {
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
    let token = env.login(&key).await;
    let (s, _) = env
        .post(
            &env.admin,
            &format!("/admin/workers/{id}/authorize"),
            json!({ "authorized": true }),
        )
        .await;
    assert_eq!(s, 204);
    Worker { key, token, id }
}

/// A project with one matmul task queued; returns (researcher token, project, input hashes).
async fn setup_job(env: &Env) -> (String, ProjectId, Vec<ArtifactRef>) {
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
            json!({ "amount": "100", "reference": "test" }),
        )
        .await;
    assert_eq!(s, 200, "{b}");
    let mut d = gg_protocol::drbg::Drbg::new("p2p", 3);
    // 96 KiB per input: more than one 64 KiB chunk.
    let a = TensorF32::new(vec![128, 192], d.sym_f32_vec(128 * 192)).unwrap();
    let b = TensorF32::new(vec![192, 64], d.sym_f32_vec(192 * 64)).unwrap();
    let mut refs = Vec::new();
    for t in [&a, &b] {
        let bytes = t.encode();
        let hash = Sha256Digest::of_bytes(&bytes);
        let size = bytes.len() as u64;
        let (s, resp) = env
            .put_bytes(
                &token,
                &format!(
                    "/researcher/projects/{project}/artifacts/{hash}?size={size}&schema=ggt1-f32"
                ),
                bytes,
            )
            .await;
        assert_eq!(s, 200, "{resp}");
        refs.push(ArtifactRef { hash, size });
    }
    // Two tasks over the same inputs: two workers can hold an assignment for the same artifacts,
    // which is exactly the situation peer distribution is for.
    let def = gg_compute::catalog::matmul_definition(128, 192, 64, refs[0], refs[1]).unwrap();
    let (s, job) = env
        .post(
            &token,
            &format!("/researcher/projects/{project}/jobs"),
            json!({ "name": "mm", "tasks": [def, def] }),
        )
        .await;
    assert_eq!(s, 200, "{job}");
    (token, project, refs)
}

async fn claim(env: &Env, w: &Worker) -> Value {
    let (s, claim) = env
        .post(
            &w.token,
            "/worker/claim",
            json!({ "kinds": ["tensor.matmul.v1"], "wait_secs": 2 }),
        )
        .await;
    assert_eq!(s, 200, "{claim}");
    claim
}

/// A transport key fingerprint announced by a worker (any 32 bytes as far as the tracker is
/// concerned: it publishes what the holder declared and readers pin it during the TLS handshake).
fn fingerprint() -> Sha256Digest {
    Sha256Digest::of_bytes(b"transport key of the holder")
}

#[tokio::test]
async fn tracker_serves_only_workers_entitled_to_the_artifact() {
    let env = start(true, 300).await;
    let (_researcher, _project, refs) = setup_job(&env).await;
    let hash = refs[0].hash;
    let holder = setup_worker(&env, "holder").await;
    let stranger = setup_worker(&env, "stranger").await;

    // Before any assignment, nothing is available to either worker.
    for path in [
        format!("/worker/artifacts/{hash}/manifest"),
        format!("/worker/artifacts/{hash}/peers"),
    ] {
        let (s, _) = env.get(&holder.token, &path).await;
        assert_eq!(s, 404, "{path} before assignment");
    }
    let (s, _) = env
        .post(
            &holder.token,
            &format!("/worker/artifacts/{hash}/announce"),
            json!({ "addr": "127.0.0.1:7100", "tls_fingerprint": fingerprint() }),
        )
        .await;
    assert_eq!(s, 404);

    // The worker that holds the assignment gets a manifest signed by the server, describing the
    // server's own bytes.
    let _claim = claim(&env, &holder).await;
    let (s, m) = env
        .get(&holder.token, &format!("/worker/artifacts/{hash}/manifest"))
        .await;
    assert_eq!(s, 200, "{m}");
    let manifest: Signed<ChunkManifestV1> = serde_json::from_value(m["manifest"].clone()).unwrap();
    manifest
        .verify(&env.network, Domain::ChunkManifest)
        .unwrap();
    assert_eq!(manifest.signer, env.server_key);
    assert_eq!(manifest.body.artifact_hash, hash);
    assert_eq!(manifest.body.size, refs[0].size);
    manifest.body.validate_structure().unwrap();
    let (s, bytes) = env
        .get_bytes(&holder.token, &format!("/worker/artifacts/{hash}"))
        .await;
    assert_eq!(s, 200);
    let recomputed: Vec<Sha256Digest> = bytes
        .chunks(manifest.body.chunk_size as usize)
        .map(Sha256Digest::of_bytes)
        .collect();
    assert_eq!(
        recomputed, manifest.body.chunks,
        "digests describe the bytes"
    );
    assert!(manifest.body.chunks.len() > 1, "the fixture spans chunks");

    // The stranger has no assignment: no manifest, no peers, no announcement.
    let (s, _) = env
        .get(
            &stranger.token,
            &format!("/worker/artifacts/{hash}/manifest"),
        )
        .await;
    assert_eq!(s, 404);
    let (s, _) = env
        .get(&stranger.token, &format!("/worker/artifacts/{hash}/peers"))
        .await;
    assert_eq!(s, 404);

    // Announcing, then reading the peer list from another entitled worker.
    let (s, announced) = env
        .post(
            &holder.token,
            &format!("/worker/artifacts/{hash}/announce"),
            json!({ "addr": "127.0.0.1:7100", "tls_fingerprint": fingerprint() }),
        )
        .await;
    assert_eq!(s, 200, "{announced}");
    // The control plane reports the address it observed, which a worker behind a NAT announces
    // instead of guessing its own (ADR-0013).
    assert_eq!(announced["observed_ip"], "127.0.0.1");
    assert_eq!(announced["ttl_secs"], 300);
    // A malformed or unroutable address never reaches another worker.
    for bad in ["not-an-address", "0.0.0.0:7100", "203.0.113.1:0"] {
        let (s, e) = env
            .post(
                &holder.token,
                &format!("/worker/artifacts/{hash}/announce"),
                json!({ "addr": bad, "tls_fingerprint": fingerprint() }),
            )
            .await;
        assert_eq!(s, 422, "address {bad} was accepted: {e}");
    }
    // The caller never sees itself in the list.
    let (s, own) = env
        .get(&holder.token, &format!("/worker/artifacts/{hash}/peers"))
        .await;
    assert_eq!(s, 200, "{own}");
    assert_eq!(own["peers"].as_array().unwrap().len(), 0);
    let ticket: Signed<PeerTicketV1> = serde_json::from_value(own["ticket"].clone()).unwrap();
    ticket.verify(&env.network, Domain::PeerTicket).unwrap();
    assert_eq!(ticket.signer, env.server_key);
    assert_eq!(ticket.body.reader_id, holder.id.into());
    assert_eq!(ticket.body.reader_key, holder.key.public_key());
    assert_eq!(ticket.body.artifact_hash, hash);
    assert_eq!(ticket.body.chunk_size, manifest.body.chunk_size);
    assert!(ticket.body.expires_at.0 > UnixMillis::now().0);

    // A second entitled worker sees the holder.
    let second = setup_worker(&env, "second").await;
    let _claim2 = claim(&env, &second).await;
    let (s, peers) = env
        .get(&second.token, &format!("/worker/artifacts/{hash}/peers"))
        .await;
    assert_eq!(s, 200, "{peers}");
    let list = peers["peers"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{peers}");
    assert_eq!(list[0]["addr"], "127.0.0.1:7100");
    assert_eq!(list[0]["worker_id"], json!(holder.id));
    assert_eq!(
        list[0]["tls_fingerprint"],
        json!(fingerprint()),
        "the reader is told which transport key to pin"
    );

    // Deauthorizing the holder withdraws it as a source.
    let (s, _) = env
        .post(
            &env.admin,
            &format!("/admin/workers/{}/authorize", holder.id),
            json!({ "authorized": false }),
        )
        .await;
    assert_eq!(s, 204);
    let (s, peers) = env
        .get(&second.token, &format!("/worker/artifacts/{hash}/peers"))
        .await;
    assert_eq!(s, 200, "{peers}");
    assert_eq!(peers["peers"].as_array().unwrap().len(), 0);
    env.stop().await;
}

#[tokio::test]
async fn announcements_expire_and_private_addresses_follow_the_policy() {
    // TTL of one second, private addresses refused.
    let env = start(false, 1).await;
    let (_researcher, _project, refs) = setup_job(&env).await;
    let hash = refs[0].hash;
    let holder = setup_worker(&env, "holder").await;
    let _claim = claim(&env, &holder).await;

    // With p2p_allow_private_addrs = false, loopback is refused: no worker is ever told to connect
    // to a local address of another host.
    let (s, e) = env
        .post(
            &holder.token,
            &format!("/worker/artifacts/{hash}/announce"),
            json!({ "addr": "127.0.0.1:7100", "tls_fingerprint": fingerprint() }),
        )
        .await;
    assert_eq!(s, 422, "{e}");
    let (s, a) = env
        .post(
            &holder.token,
            &format!("/worker/artifacts/{hash}/announce"),
            json!({ "addr": "203.0.113.5:7100", "tls_fingerprint": fingerprint() }),
        )
        .await;
    assert_eq!(s, 200, "{a}");

    let second = setup_worker(&env, "second").await;
    let _c = claim(&env, &second).await;
    let (_s, peers) = env
        .get(&second.token, &format!("/worker/artifacts/{hash}/peers"))
        .await;
    assert_eq!(peers["peers"].as_array().unwrap().len(), 1);

    // After the TTL the announcement is gone, with or without the reaper pass.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let (_s, peers) = env
        .get(&second.token, &format!("/worker/artifacts/{hash}/peers"))
        .await;
    assert_eq!(
        peers["peers"].as_array().unwrap().len(),
        0,
        "expired announcements are not served"
    );
    env.stop().await;
}

#[tokio::test]
async fn peer_distribution_can_be_switched_off_server_side() {
    let tdb = TestDb::create_or_panic().await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = ServerConfig {
        database_url: tdb.url(),
        data_dir: tmp.path().to_path_buf(),
        network_id: "gg-e2e".into(),
        p2p_enabled: false,
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
    let env = Env {
        base: running.url(),
        running: Some(running),
        tdb: Some(tdb),
        http: reqwest::Client::new(),
        admin,
        network: NetworkId::new("gg-e2e").unwrap(),
        server_key: PublicKey([0; 32]),
        _tmp: tmp,
    };
    let (_researcher, _project, refs) = setup_job(&env).await;
    let hash = refs[0].hash;
    let holder = setup_worker(&env, "holder").await;
    let _claim = claim(&env, &holder).await;
    // Entitled worker, but the feature is off: no manifest, no peers, no announcements — while
    // plain HTTP downloads keep working.
    let (s, _) = env
        .get(&holder.token, &format!("/worker/artifacts/{hash}/manifest"))
        .await;
    assert_eq!(s, 404);
    let (s, _) = env
        .get(&holder.token, &format!("/worker/artifacts/{hash}/peers"))
        .await;
    assert_eq!(s, 404);
    let (s, _) = env
        .post(
            &holder.token,
            &format!("/worker/artifacts/{hash}/announce"),
            json!({ "addr": "203.0.113.5:7100", "tls_fingerprint": fingerprint() }),
        )
        .await;
    assert_eq!(s, 404);
    let (s, bytes) = env
        .get_bytes(&holder.token, &format!("/worker/artifacts/{hash}"))
        .await;
    assert_eq!(s, 200);
    assert_eq!(bytes.len() as u64, refs[0].size);
    env.stop().await;
}

/// Unused import guard: `BTreeMap` documents that specs carry named inputs.
#[allow(dead_code)]
fn _inputs_are_named(_: BTreeMap<String, ArtifactRef>) {}

#[tokio::test]
async fn an_unreachable_announced_address_is_never_advertised() {
    let env = start_with(true, 300, true).await;
    let (_researcher, _project, refs) = setup_job(&env).await;
    let hash = refs[0].hash;
    let holder = setup_worker(&env, "holder").await;
    let _claim = claim(&env, &holder).await;

    // Nothing listens there: a worker behind a NAT without a usable mapping is not advertised.
    let (s, e) = env
        .post(
            &holder.token,
            &format!("/worker/artifacts/{hash}/announce"),
            json!({ "addr": "127.0.0.1:9", "tls_fingerprint": fingerprint() }),
        )
        .await;
    assert_eq!(s, 422, "{e}");

    // A real listener is accepted and advertised.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            if listener.accept().await.is_err() {
                break;
            }
        }
    });
    let (s, a) = env
        .post(
            &holder.token,
            &format!("/worker/artifacts/{hash}/announce"),
            json!({ "addr": addr.to_string(), "tls_fingerprint": fingerprint() }),
        )
        .await;
    assert_eq!(s, 200, "{a}");
    let second = setup_worker(&env, "second").await;
    let _c = claim(&env, &second).await;
    let (_s, peers) = env
        .get(&second.token, &format!("/worker/artifacts/{hash}/peers"))
        .await;
    let list = peers["peers"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{peers}");
    assert_eq!(list[0]["addr"], addr.to_string());
    env.stop().await;
}

#[tokio::test]
async fn ipv6_peers_are_announced_and_advertised_with_brackets() {
    // Dial-back off: the test asserts the policy and the storage form, not reachability.
    let env = start_with(false, 300, false).await;
    let (_researcher, _project, refs) = setup_job(&env).await;
    let hash = refs[0].hash;
    let holder = setup_worker(&env, "holder").await;
    let _claim = claim(&env, &holder).await;

    // A global IPv6 address needs no NAT traversal and is accepted as is.
    let (s, a) = env
        .post(
            &holder.token,
            &format!("/worker/artifacts/{hash}/announce"),
            json!({ "addr": "[2a01:cb19:1::1]:7100", "tls_fingerprint": fingerprint() }),
        )
        .await;
    assert_eq!(s, 200, "{a}");

    // Loopback, link-local and unique-local IPv6 are refused by the same policy as IPv4.
    for bad in [
        "[::1]:7100",
        "[fe80::1]:7100",
        "[fd00::1]:7100",
        "[::]:7100",
    ] {
        let (s, e) = env
            .post(
                &holder.token,
                &format!("/worker/artifacts/{hash}/announce"),
                json!({ "addr": bad, "tls_fingerprint": fingerprint() }),
            )
            .await;
        assert_eq!(s, 422, "address {bad} was accepted: {e}");
    }

    // The reader gets the bracketed form it can connect to directly.
    let second = setup_worker(&env, "second").await;
    let _c = claim(&env, &second).await;
    let (_s, peers) = env
        .get(&second.token, &format!("/worker/artifacts/{hash}/peers"))
        .await;
    let list = peers["peers"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{peers}");
    assert_eq!(list[0]["addr"], "[2a01:cb19:1::1]:7100");
    env.stop().await;
}

/// Direct hand-off of an activation (ADR-0026): the worker that produced and uploaded an output may
/// announce itself as a holder, so that the next stage fetches it from there; another worker, which
/// neither produced it nor received it as an input, may not.
#[tokio::test]
async fn a_producer_may_announce_its_own_output_and_only_it() {
    let env = start(true, 300).await;
    let (_researcher, _project, _refs) = setup_job(&env).await;
    let producer = setup_worker(&env, "producer").await;
    let stranger = setup_worker(&env, "stranger").await;
    let c = claim(&env, &producer).await;
    let attempt = c["assignment"]["body"]["attempt_id"].as_str().unwrap();
    let out = TensorF32::new(vec![4, 4], vec![0.5; 16]).unwrap().encode();
    let hash = Sha256Digest::of_bytes(&out);
    let (s, r) = env
        .put_bytes(
            &producer.token,
            &format!(
                "/worker/attempts/{attempt}/outputs/{hash}?size={}&schema=ggt1-f32",
                out.len()
            ),
            out,
        )
        .await;
    assert_eq!(s, 200, "{r}");
    let path = format!("/worker/artifacts/{hash}/announce");
    let body = json!({ "addr": "127.0.0.1:7200", "tls_fingerprint": fingerprint() });
    let (s, a) = env.post(&producer.token, &path, body.clone()).await;
    assert_eq!(s, 200, "{a}");
    let (s, _) = env.post(&stranger.token, &path, body).await;
    assert_eq!(
        s, 404,
        "a stranger worker cannot announce an output it did not produce"
    );
    env.stop().await;
}
