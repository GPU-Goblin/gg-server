//! The lab node (ADR-0041): the scientist's machine holds the artifacts of its runs and serves them
//! to the contributors' workers, so the control plane stores hashes and chunk digests, never the
//! bytes.
//!
//! What a node does today (milestone A3):
//! - signs in with its own key, registered by its researcher (`POST /researcher/nodes`);
//! - takes a file into its store, hashing the bytes it keeps (whole and per chunk), and declares
//!   them for a project;
//! - serves their chunks over the peer protocol (TLS with a pinned raw key, server-signed tickets,
//!   ADR-0012/0013) and announces itself as their holder, renewing the announcement before it
//!   expires;
//! - pulls the outputs of the jobs it holds from the workers that produced them, checks every
//!   chunk against the manifest the server signs and the whole against its hash, then tells the
//!   server it holds them (milestone A4).
//!
//! Readers check every chunk against the manifest the server signs from these digests and the
//! whole against the hash: a node can make its own researcher's runs fail, nothing more.

pub mod api;
mod compare;
pub mod config;
pub mod directory;
pub mod service;
mod step;

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use gg_client::cache::ArtifactCache;
use gg_client::client::{Api, ClientError, ServerInfo};
use gg_client::p2p::PeerServer;
use gg_core::{NodeId, ProjectId, Sha256Digest};
use gg_protocol::keys::{PublicKey, SecretKey};
use gg_protocol::messages::ArtifactRef;
use serde::Deserialize;
use tokio::sync::Mutex;

/// How a node is set up.
#[derive(Clone, Debug)]
pub struct NodeConfig {
    /// Control plane base URL.
    pub server_url: String,
    /// Allow `http://` to a non-loopback server (development only).
    pub allow_plain_http: bool,
    /// Where the node keeps what it holds (`<state_dir>/store`).
    pub state_dir: PathBuf,
    /// Address the chunk server binds (`0.0.0.0:0` picks a port).
    pub listen_addr: String,
    /// How the chunk server makes itself reachable: a configured address, IPv6, a gateway
    /// mapping, the bound or the observed address (A8).
    pub reach: gg_client::reach::ReachConfig,
    /// Concurrent peer connections served.
    pub max_conns: u32,
    /// Accept loopback and private addresses for workers (a laboratory network, tests).
    pub allow_private_peers: bool,
    /// How long pulling an output may keep failing before the node gives up on it: its producer
    /// is gone or serves wrong bytes, and its task is better sent again than left waiting for the
    /// server's deadline.
    pub give_up_after: Duration,
    /// The network and signing key the server must present (from the signed list of servers):
    /// a server that presents others is refused.
    pub expect: Option<(String, PublicKey)>,
}

/// An output waiting for this node.
#[derive(Clone, Debug, Deserialize)]
struct PullItem {
    hash: Sha256Digest,
    size: u64,
}

/// The server's answer to a declaration.
#[derive(Clone, Debug, Deserialize)]
pub struct Held {
    /// Hash.
    pub hash: Sha256Digest,
    /// Size.
    pub size: u64,
    /// Chunk size of the manifest.
    pub chunk_size: u32,
    /// The server stores these bytes itself (an earlier upload): nothing to serve.
    pub server_holds: bool,
}

/// Outputs pulled at once.
const PULLS_AT_ONCE: usize = 8;

/// A running lab node.
pub struct Node {
    api: Api,
    key: SecretKey,
    info: ServerInfo,
    id: NodeId,
    token: Mutex<String>,
    cache: ArtifactCache,
    server: PeerServer,
    reach: gg_client::reach::Reach,
    allow_private_peers: bool,
    give_up_after: Duration,
    /// Outputs being pulled by [`Node::keep_pulling`].
    in_flight: std::sync::Mutex<std::collections::BTreeSet<Sha256Digest>>,
    /// Outputs whose pull keeps failing, with the first failure.
    failing: std::sync::Mutex<BTreeMap<Sha256Digest, std::time::Instant>>,
    /// What this node serves, by hash, with the size.
    held: Mutex<BTreeMap<Sha256Digest, u64>>,
}

impl Node {
    /// Sign in and start serving. The key must have been registered by the node's researcher.
    pub async fn start(cfg: NodeConfig, key: SecretKey) -> anyhow::Result<Arc<Self>> {
        let api = if cfg.allow_plain_http {
            Api::new_allowing_plain_http(&cfg.server_url)?
        } else {
            Api::new(&cfg.server_url)?
        };
        let info = api.info().await.context("server info")?;
        if let Some((network, key)) = &cfg.expect
            && (info.network_id.as_str() != network || &info.server_public_key != key)
        {
            bail!(
                "{} presents network {} and another key than the server list names",
                cfg.server_url,
                info.network_id
            );
        }
        if info.p2p_chunk_bytes == 0 {
            bail!(
                "the server does not distribute artifacts through peers: a lab node cannot serve"
            );
        }
        let session = api
            .login(&key, &info.network_id, "node-login")
            .await
            .context("node sign-in (is this key registered by its researcher?)")?;
        let store = cfg.state_dir.join("store");
        std::fs::create_dir_all(&store)?;
        // Unbounded: what the node holds is the only copy of its runs' artifacts.
        let cache = ArtifactCache::new(store, u64::MAX);
        let server = PeerServer::start(
            &cfg.listen_addr,
            cache.clone(),
            info.network_id.clone(),
            info.server_public_key,
            cfg.max_conns,
            u64::MAX,
        )
        .await?;
        Ok(Arc::new(Self {
            api,
            key,
            info,
            id: NodeId(session.principal_id),
            token: Mutex::new(session.token),
            cache,
            server,
            reach: gg_client::reach::Reach::new(cfg.reach.clone()),
            allow_private_peers: cfg.allow_private_peers,
            give_up_after: cfg.give_up_after,
            failing: std::sync::Mutex::new(BTreeMap::new()),
            in_flight: std::sync::Mutex::new(std::collections::BTreeSet::new()),
            held: Mutex::new(BTreeMap::new()),
        }))
    }

    /// This node's record.
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// The address the control plane accepted for this node's chunk server, and how it was
    /// obtained (`configured`, `ipv6`, `nat-pmp`, `upnp`, `bound`, `observed`); `None` while the
    /// node has announced nothing, or when no address could be reached.
    pub async fn reachable_at(&self) -> Option<(String, &'static str)> {
        self.reach.current().await
    }

    /// Chunks served so far.
    pub fn chunks_served(&self) -> u64 {
        self.server.stats().snapshot().0
    }

    /// Take the file at `path` into the store, declare it for `project`, and serve it. The digests
    /// are computed over the copy the node keeps, not over `path`, which the scientist may change
    /// afterwards.
    pub async fn hold(
        &self,
        project: ProjectId,
        path: &Path,
        schema: &str,
    ) -> anyhow::Result<ArtifactRef> {
        let chunk = self.info.p2p_chunk_bytes as usize;
        let tmp_dir = self.cache.root().join(".incoming");
        std::fs::create_dir_all(&tmp_dir)?;
        let src = path.to_path_buf();
        let (tmp, hash, size, chunks) =
            tokio::task::spawn_blocking(move || copy_and_digest(&src, &tmp_dir, chunk)).await??;
        self.keep_and_declare(project, tmp, (hash, size, chunks), schema)
            .await
    }

    /// Hold bytes the node produced itself (a checkpoint, optimizer state), as [`Node::hold`].
    pub async fn hold_bytes(
        &self,
        project: ProjectId,
        bytes: Vec<u8>,
        schema: &str,
    ) -> anyhow::Result<ArtifactRef> {
        let chunk = self.info.p2p_chunk_bytes as usize;
        let tmp_dir = self.cache.root().join(".incoming");
        std::fs::create_dir_all(&tmp_dir)?;
        let (tmp, digest) = tokio::task::spawn_blocking(move || {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp = tmp_dir.join(format!("own-{}-{seq}", std::process::id()));
            let chunks: Vec<Sha256Digest> =
                bytes.chunks(chunk).map(Sha256Digest::of_bytes).collect();
            let hash = Sha256Digest::of_bytes(&bytes);
            let mut f = std::fs::File::create(&tmp)?;
            std::io::Write::write_all(&mut f, &bytes)?;
            f.sync_all()?;
            anyhow::Ok((tmp, (hash, bytes.len() as u64, chunks)))
        })
        .await??;
        self.keep_and_declare(project, tmp, digest, schema).await
    }

    /// Move a file whose digests are known into the store, declare it, and serve it.
    async fn keep_and_declare(
        &self,
        project: ProjectId,
        tmp: PathBuf,
        (hash, size, chunks): (Sha256Digest, u64, Vec<Sha256Digest>),
        schema: &str,
    ) -> anyhow::Result<ArtifactRef> {
        // The first bytes, so that the server knows a dataset's shape without holding it.
        let header = {
            let mut f = std::fs::File::open(&tmp)?;
            let mut buf = vec![0u8; 64.min(size as usize)];
            f.read_exact(&mut buf)?;
            buf
        };
        let stored = self.cache.store(&hash, &tmp);
        let _ = std::fs::remove_file(&tmp);
        stored?;
        self.cache.pin(&hash);
        let held: Held = self
            .call(|token| {
                let body = serde_json::json!({
                    "project_id": project,
                    "size": size,
                    "chunks": chunks,
                    "schema": schema,
                    "header": hex_encode(&header),
                });
                let path = format!("/node/artifacts/{hash}");
                async move { self.api.post_json(&token, &path, &body).await }
            })
            .await?
            .context("empty answer to a declaration")?;
        if held.hash != hash || held.size != size {
            bail!("the server recorded another artifact than the one declared");
        }
        if !held.server_holds {
            self.announce_or_warn(&hash).await;
            self.held.lock().await.insert(hash, size);
        }
        Ok(ArtifactRef { hash, size })
    }

    /// Whether this node holds (and serves) `hash`.
    pub async fn holds(&self, hash: &Sha256Digest) -> bool {
        self.held.lock().await.contains_key(hash)
    }

    /// Path of a held artifact in the node's store.
    pub fn path_of(&self, hash: &Sha256Digest) -> PathBuf {
        self.cache.path_of(hash)
    }

    /// Pull the outputs waiting for this node: each from the workers holding it, checked chunk by
    /// chunk against the server-signed manifest and whole against its hash, then kept, declared
    /// held and served. Returns how many were pulled; one that fails is retried next time.
    pub async fn pull_once(&self) -> anyhow::Result<usize> {
        let items: Vec<PullItem> = self
            .call(|token| async move { self.api.get_json(&token, "/node/pulls").await })
            .await?;
        // Several at once: one producer that is gone or lies must not hold up the others.
        use futures_util::StreamExt;
        let outcomes: Vec<anyhow::Result<bool>> = futures_util::stream::iter(items)
            .map(|it| self.pull_or_give_up(it))
            .buffer_unordered(PULLS_AT_ONCE)
            .collect()
            .await;
        let mut pulled = 0;
        for o in outcomes {
            if o? {
                pulled += 1;
            }
        }
        Ok(pulled)
    }

    /// Pull one output; past `give_up_after` of failures, tell the server the node gives up on
    /// it. Returns whether it was pulled.
    async fn pull_or_give_up(&self, it: PullItem) -> anyhow::Result<bool> {
        let e = match self.pull(&it).await {
            Ok(()) => {
                self.failing
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&it.hash);
                return Ok(true);
            }
            Err(e) => e,
        };
        let since = *self
            .failing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(it.hash)
            .or_insert_with(std::time::Instant::now);
        // A producer whose bytes contradict the digests it declared itself is not an honest failure:
        // no point waiting for it.
        let lied = matches!(
            e.downcast_ref::<gg_client::client::ClientError>(),
            Some(gg_client::client::ClientError::Integrity(_))
        );
        if since.elapsed() < self.give_up_after && !lied {
            tracing::warn!(hash = %it.hash, error = %e, "pulling an output failed, retrying later");
            return Ok(false);
        }
        tracing::warn!(hash = %it.hash, error = %e, "giving up on an output: its task is sent again");
        let path = format!("/node/artifacts/{}/unavailable", it.hash);
        self.call(|token| {
            let path = path.clone();
            async move {
                self.api
                    .post_json::<serde_json::Value>(&token, &path, &serde_json::json!({}))
                    .await
            }
        })
        .await?;
        self.failing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&it.hash);
        Ok(false)
    }

    async fn pull(&self, it: &PullItem) -> anyhow::Result<()> {
        let artifact = ArtifactRef {
            hash: it.hash,
            size: it.size,
        };
        if !self.cache.path_of(&it.hash).exists() {
            let dir = self.cache.root().join(".incoming");
            std::fs::create_dir_all(&dir)?;
            // Its own file: two attempts may produce the same bytes, pulled at the same time.
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dest = dir.join(format!("pull-{}-{seq}", it.hash));
            let token = self.token.lock().await.clone();
            // Nobody announced it (its producer cannot be reached, or has not announced yet): the
            // producer is asked to push it here instead (A8).
            let peers: serde_json::Value = self
                .api
                .get_json(&token, &format!("/node/artifacts/{}/peers", it.hash))
                .await?;
            let nobody = peers["peers"].as_array().is_none_or(|p| p.is_empty())
                && peers["server_holds"] == serde_json::json!(false);
            if nobody {
                self.receive_by_push(&token, &artifact, &dest).await?;
            } else {
                gg_client::p2p::fetch_with_peers(
                    &self.api,
                    &token,
                    &self.key,
                    &self.info.network_id,
                    &self.info.server_public_key,
                    &artifact,
                    &format!("/node/artifacts/{}", it.hash),
                    &dest,
                    &gg_client::p2p::FetchConfig {
                        allow_private_peers: self.allow_private_peers,
                        partials: Some(self.server.partials()),
                        tracker: "/node".into(),
                        ..Default::default()
                    },
                )
                .await?;
            }
            let stored = self.cache.store(&it.hash, &dest);
            let _ = std::fs::remove_file(&dest);
            stored?;
        }
        self.cache.pin(&it.hash);
        let path = format!("/node/artifacts/{}/pulled", it.hash);
        let held: Held = self
            .call(|token| {
                let path = path.clone();
                async move {
                    self.api
                        .post_json(&token, &path, &serde_json::json!({}))
                        .await
                }
            })
            .await?
            .context("empty answer to a pull")?;
        if held.hash != it.hash || held.size != it.size {
            bail!("the server recorded another artifact than the one pulled");
        }
        self.held.lock().await.insert(it.hash, it.size);
        self.announce_or_warn(&it.hash).await;
        Ok(())
    }

    /// Start pulling each waiting output not already being pulled, each on its own (a producer
    /// that is gone or lies holds up nobody else), as soon as the server lists it (it answers a
    /// waiting request at once), until `stop` turns true. `period`: the pause while everything
    /// listed is already being pulled, and after a failure.
    pub async fn keep_pulling(
        self: Arc<Self>,
        period: Duration,
        mut stop: tokio::sync::watch::Receiver<bool>,
    ) {
        loop {
            let me: &Self = &self;
            let listed = tokio::select! {
                r = me.call(|token| async move {
                    me.api.get_json::<Vec<PullItem>>(&token, "/node/pulls?wait_secs=10").await
                }) => r,
                _ = stop.changed() => { if *stop.borrow() { return; } continue; }
            };
            let items = match listed {
                Ok(items) => items,
                Err(e) => {
                    tracing::warn!(error = %e, "listing the outputs to pull failed");
                    tokio::time::sleep(period).await;
                    continue;
                }
            };
            let mut started = 0;
            for it in items {
                if !self
                    .in_flight
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(it.hash)
                {
                    continue;
                }
                started += 1;
                let node = self.clone();
                tokio::spawn(async move {
                    let hash = it.hash;
                    if let Err(e) = node.pull_or_give_up(it).await {
                        tracing::warn!(%hash, error = %e, "pulling an output failed");
                    }
                    node.in_flight
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&hash);
                });
            }
            if started == 0 {
                // Everything listed is being pulled: the list changes when a pull ends.
                tokio::time::sleep(period).await;
            }
        }
    }

    /// Receive `artifact` by push: its holder connects here (A8). This node's addresses are offered
    /// in order (the one accepted before, then every candidate); the control plane dials back the
    /// first it can use.
    async fn receive_by_push(
        &self,
        token: &str,
        artifact: &ArtifactRef,
        dest: &Path,
    ) -> anyhow::Result<()> {
        let mut addrs: Vec<String> = self
            .reach
            .current()
            .await
            .map(|(a, _)| a)
            .into_iter()
            .collect();
        let observed = async {
            let me: serde_json::Value = self.api.get_json(token, "/node/me").await.ok()?;
            me["observed_ip"].as_str()?.parse().ok()
        };
        for c in self.reach.candidates(self.server.addr(), observed).await {
            if !addrs.contains(&c.addr) {
                addrs.push(c.addr);
            }
        }
        gg_client::p2p::fetch_by_push(
            &self.api,
            token,
            "/node",
            &self.info.network_id,
            &self.info.server_public_key,
            artifact,
            dest,
            &self.server,
            &addrs,
            Duration::from_secs(30),
        )
        .await?;
        Ok(())
    }

    /// Push what readers that cannot reach this node ask for, over connections this node opens to
    /// them (A8), until `stop` turns true.
    pub async fn keep_pushing(
        self: Arc<Self>,
        retry: Duration,
        mut stop: tokio::sync::watch::Receiver<bool>,
    ) {
        loop {
            let me: &Self = &self;
            let asked = tokio::select! {
                r = me.call(|t| async move {
                    me.api.get_json::<Vec<gg_client::p2p::PushRequest>>(&t, "/node/pushes?wait_secs=10").await
                }) => r,
                _ = stop.changed() => { if *stop.borrow() { return; } continue; }
            };
            let items = match asked {
                Ok(items) => items,
                Err(e) => {
                    tracing::warn!(error = %e, "waiting for push requests failed");
                    tokio::time::sleep(retry).await;
                    continue;
                }
            };
            for req in items {
                let cache = self.cache.clone();
                tokio::spawn(async move {
                    match gg_client::p2p::push_artifact(&cache, &req, Duration::from_secs(20)).await
                    {
                        Ok(n) => {
                            tracing::info!(hash = %req.hash, reader = %req.reader_addr, chunks = n, "pushed to a reader")
                        }
                        Err(e) => {
                            tracing::warn!(hash = %req.hash, reader = %req.reader_addr, error = %e, "push failed")
                        }
                    }
                });
            }
        }
    }

    /// Relay chunks through the control plane for the readers that cannot reach this node (A8):
    /// wait for requests, send each chunk from the store, until `stop` turns true.
    pub async fn keep_relaying(
        self: Arc<Self>,
        retry: Duration,
        mut stop: tokio::sync::watch::Receiver<bool>,
    ) {
        #[derive(Deserialize)]
        struct Item {
            request_id: uuid::Uuid,
            hash: Sha256Digest,
            start: u64,
            len: u64,
        }
        loop {
            let me: &Self = &self;
            let asked = tokio::select! {
                r = me.call(|t| async move { me.api.get_json::<Vec<Item>>(&t, "/node/relay?wait_secs=10").await }) => r,
                _ = stop.changed() => { if *stop.borrow() { return; } continue; }
            };
            let items = match asked {
                Ok(items) => items,
                Err(e) => {
                    tracing::warn!(error = %e, "waiting for relay requests failed");
                    tokio::time::sleep(retry).await;
                    continue;
                }
            };
            for it in items {
                let node = self.clone();
                tokio::spawn(async move {
                    let Some(size) = node.held.lock().await.get(&it.hash).copied() else {
                        tracing::warn!(hash = %it.hash, "asked to relay an artifact this node does not hold");
                        return;
                    };
                    let bytes = match node.cache.read_range(&it.hash, size, it.start, it.len) {
                        Ok(Some(b)) => b,
                        other => {
                            tracing::warn!(hash = %it.hash, result = ?other.map(|o| o.is_some()), "cannot read the chunk to relay");
                            return;
                        }
                    };
                    let path = format!("/node/relay/{}", it.request_id);
                    let sent = node
                        .call(|t| {
                            let (path, bytes) = (path.clone(), bytes.clone());
                            let node = &node;
                            async move { node.api.put_bytes(&t, &path, bytes).await }
                        })
                        .await;
                    if let Err(e) = sent {
                        tracing::debug!(error = %e, "relayed chunk not taken (the reader gave up?)");
                    }
                });
            }
        }
    }

    /// Announce every artifact held, again: the control plane forgets announcements after its
    /// peer TTL.
    pub async fn announce_all(&self) -> anyhow::Result<()> {
        let hashes: Vec<Sha256Digest> = self.held.lock().await.keys().copied().collect();
        for hash in hashes {
            self.announce_or_warn(&hash).await;
        }
        Ok(())
    }

    /// Renew the announcements every `period` until `stop` turns true.
    pub async fn keep_announcing(
        self: Arc<Self>,
        period: Duration,
        mut stop: tokio::sync::watch::Receiver<bool>,
    ) {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(period) => {
                    if let Err(e) = self.announce_all().await {
                        tracing::warn!(error = %e, "renewing the node's announcements failed");
                    }
                }
                _ = stop.changed() => { if *stop.borrow() { return; } }
            }
        }
    }

    /// Announce, or say why readers will go through the control plane's relay instead (small
    /// artifacts) or not reach it at all: an unreachable node still holds and serves what it can.
    async fn announce_or_warn(&self, hash: &Sha256Digest) {
        if let Err(e) = self.announce(hash).await {
            tracing::warn!(%hash, error = %format!("{e:#}"), "not announced");
        }
    }

    async fn announce(&self, hash: &Sha256Digest) -> anyhow::Result<()> {
        let fingerprint = self.server.fingerprint();
        let observed = async {
            let me: serde_json::Value = self
                .call(|t| async move { self.api.get_json(&t, "/node/me").await })
                .await
                .ok()?;
            me["observed_ip"].as_str()?.parse().ok()
        };
        let accepted = self
            .reach
            .announce(self.server.addr(), observed, |addr| async move {
                let body = serde_json::json!({ "addr": addr, "tls_fingerprint": fingerprint });
                let path = format!("/node/artifacts/{hash}/announce");
                self.call(|token| {
                    let (path, body) = (path.clone(), body.clone());
                    async move {
                        self.api
                            .post_json::<serde_json::Value>(&token, &path, &body)
                            .await
                    }
                })
                .await
                .map(|_| ())
                .map_err(|e| format!("{e:#}"))
            })
            .await;
        if accepted.is_none() {
            bail!(
                "no address of this node is reachable from the control plane: workers cannot fetch \
                 what it holds (forward the port of p2p_listen, use IPv6, or set p2p_advertise)"
            );
        }
        Ok(())
    }

    /// One call with the current session, signing in again once if it has expired.
    async fn call<T, F, Fut>(&self, f: F) -> anyhow::Result<T>
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = Result<T, ClientError>>,
    {
        let token = self.token.lock().await.clone();
        match f(token).await {
            Err(ClientError::Status { status: 401, .. }) => {
                let session = self
                    .api
                    .login(&self.key, &self.info.network_id, "node-login")
                    .await?;
                *self.token.lock().await = session.token.clone();
                Ok(f(session.token).await?)
            }
            other => Ok(other?),
        }
    }

    /// Stop serving.
    pub async fn stop(self: Arc<Self>) {
        // A port opened on the gateway is closed, whoever still holds the node.
        self.reach.release().await;
        match Arc::try_unwrap(self) {
            Ok(node) => node.server.stop().await,
            Err(_) => {
                tracing::warn!("node still in use at stop: its chunk server ends with the process")
            }
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Copy `src` into `dir`, hashing the bytes written: whole and per chunk of `chunk` bytes.
fn copy_and_digest(
    src: &Path,
    dir: &Path,
    chunk: usize,
) -> anyhow::Result<(PathBuf, Sha256Digest, u64, Vec<Sha256Digest>)> {
    use std::io::Write;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!("{}-{seq}", std::process::id()));
    let mut input = std::fs::File::open(src).with_context(|| format!("open {}", src.display()))?;
    let mut out = std::fs::File::create(&tmp)?;
    let mut whole = Sha256Digest::hasher();
    let mut chunks = Vec::new();
    let mut buf = vec![0u8; chunk];
    let mut size = 0u64;
    loop {
        let mut filled = 0;
        while filled < buf.len() {
            let n = input.read(&mut buf[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            break;
        }
        whole.update(&buf[..filled]);
        chunks.push(Sha256Digest::of_bytes(&buf[..filled]));
        out.write_all(&buf[..filled])?;
        size += filled as u64;
        if filled < buf.len() {
            break;
        }
    }
    out.sync_all()?;
    Ok((tmp, whole.finalize(), size, chunks))
}

#[cfg(test)]
mod tests {
    use super::copy_and_digest;
    use gg_core::Sha256Digest;

    #[test]
    fn digests_cover_the_copy_chunk_by_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let bytes: Vec<u8> = (0..250_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &bytes).unwrap();
        let (tmp, hash, size, chunks) = copy_and_digest(&src, dir.path(), 65_536).unwrap();
        assert_eq!(std::fs::read(&tmp).unwrap(), bytes);
        assert_eq!(hash, Sha256Digest::of_bytes(&bytes));
        assert_eq!(size, bytes.len() as u64);
        let expected: Vec<Sha256Digest> =
            bytes.chunks(65_536).map(Sha256Digest::of_bytes).collect();
        assert_eq!(chunks, expected);
    }
}
