//! A control plane on a real PostgreSQL, researchers, lab nodes and hand-driven workers.

#![allow(dead_code)]

use gg_control::config::ServerConfig;
use gg_control::server::{Running, serve};
use gg_control::state::AppState;
use gg_core::*;
use gg_node::{Node, NodeConfig};
use gg_protocol::keys::SecretKey;
use gg_storage::queries::identities;
use gg_storage::testing::TestDb;
use serde_json::{Value, json};

pub struct Env {
    tdb: Option<TestDb>,
    running: Option<Running>,
    http: reqwest::Client,
    pub base: String,
    pub admin: String,
    _tmp: tempfile::TempDir,
}

pub async fn start() -> Env {
    start_with(|_| {}).await
}

/// A control plane on a fresh database, with `tune` applied to its configuration.
pub async fn start_with(tune: impl FnOnce(&mut ServerConfig)) -> Env {
    let tdb = TestDb::create_or_panic().await;
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = ServerConfig {
        database_url: tdb.url(),
        data_dir: tmp.path().to_path_buf(),
        network_id: "gg-e2e".into(),
        lease_secs: 120,
        lease_max_secs: 3600,
        reaper_interval_secs: 1,
        claim_wait_max_secs: 5,
        p2p_allow_private_addrs: true,
        p2p_chunk_bytes: 64 * 1024,
        // The node really listens: the control plane checks it answers before advertising it.
        p2p_dial_back: true,
        ..ServerConfig::default()
    };
    tune(&mut cfg);
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
        _tmp: tmp,
    }
}

impl Env {
    /// The test database.
    pub fn db(&self) -> &gg_storage::Db {
        &self.tdb.as_ref().unwrap().db
    }
    pub async fn stop(mut self) {
        if let Some(r) = self.running.take() {
            r.stop().await;
        }
        if let Some(t) = self.tdb.take() {
            t.drop().await;
        }
    }
    pub async fn post(&self, token: &str, path: &str, body: Value) -> (u16, Value) {
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
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }
    pub async fn get(&self, token: &str, path: &str) -> (u16, Value) {
        let r = self
            .http
            .get(format!("{}/v1{path}", self.base))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }
}

/// An approved researcher with a funded project allowed to run matmul.
pub async fn researcher(env: &Env, name: &str) -> (String, ProjectId) {
    let (s, r) = env
        .post(
            &env.admin,
            "/admin/researchers",
            json!({ "display_name": name, "admission": "approved" }),
        )
        .await;
    assert_eq!(s, 200, "{r}");
    let token = r["token"].as_str().unwrap().to_owned();
    let id: ResearcherId = r["researcher_id"].as_str().unwrap().parse().unwrap();
    let (s, p) = env
        .post(&env.admin, "/admin/projects", json!({ "name": name, "allowed_kinds": ["tensor.matmul.v1", "graph.grad.v1", "graph.stage.v1"], "owner_researcher_id": id, "data_policy_ack": true }))
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
    (token, project)
}

/// A node registered by the researcher and attached to `project`.
pub async fn start_node(
    env: &Env,
    researcher_token: &str,
    project: ProjectId,
    dir: &std::path::Path,
) -> std::sync::Arc<Node> {
    start_node_with(
        env,
        researcher_token,
        project,
        dir,
        std::time::Duration::from_secs(30),
    )
    .await
}

/// A node that gives up on an output after `give_up_after` of failed pulls.
pub async fn start_node_with(
    env: &Env,
    researcher_token: &str,
    project: ProjectId,
    dir: &std::path::Path,
    give_up_after: std::time::Duration,
) -> std::sync::Arc<Node> {
    start_node_keyed(
        env,
        researcher_token,
        project,
        dir,
        give_up_after,
        SecretKey::generate().unwrap(),
    )
    .await
}

/// A node with the key `key` (to log in as the node from the test).
pub async fn start_node_keyed(
    env: &Env,
    researcher_token: &str,
    project: ProjectId,
    dir: &std::path::Path,
    give_up_after: std::time::Duration,
    key: SecretKey,
) -> std::sync::Arc<Node> {
    let (s, n) = env
        .post(
            researcher_token,
            "/researcher/nodes",
            json!({ "public_key": key.public_key().to_hex(), "name": "lab" }),
        )
        .await;
    assert_eq!(s, 200, "{n}");
    let (s, a) = env
        .post(
            researcher_token,
            &format!("/researcher/projects/{project}/nodes"),
            json!({ "node_id": n["node_id"] }),
        )
        .await;
    assert_eq!(s, 200, "{a}");
    Node::start(
        NodeConfig {
            server_url: env.base.clone(),
            allow_plain_http: true,
            state_dir: dir.to_path_buf(),
            listen_addr: "127.0.0.1:0".into(),
            reach: Default::default(),
            max_conns: 8,
            allow_private_peers: true,
            give_up_after,
            expect: None,
        },
        key,
    )
    .await
    .unwrap()
}

pub struct Worker {
    pub key: SecretKey,
    pub token: String,
    pub id: WorkerId,
}

pub async fn worker(env: &Env) -> Worker {
    let key = SecretKey::generate().unwrap();
    let (s, reg) = env
        .post(
            "",
            "/worker/register",
            json!({ "public_key": key.public_key().to_hex(), "display_name": "w" }),
        )
        .await;
    assert_eq!(s, 200, "{reg}");
    let id: WorkerId = reg["worker_id"].as_str().unwrap().parse().unwrap();
    let (s, _) = env
        .post(
            &env.admin,
            &format!("/admin/workers/{id}/authorize"),
            json!({ "authorized": true }),
        )
        .await;
    assert_eq!(s, 204);
    let api = gg_client::client::Api::new_allowing_plain_http(&env.base).unwrap();
    let network = NetworkId::new("gg-e2e").unwrap();
    let token = api
        .login(&key, &network, "worker-login")
        .await
        .unwrap()
        .token;
    Worker { key, token, id }
}

/// Attach `worker` to the contributor account `account` (created if new).
pub async fn join(env: &Env, worker: WorkerId, account: uuid::Uuid) {
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
        gg_storage::queries::contributors::attach_worker(&mut c, worker, account)
            .await
            .unwrap()
    );
}
