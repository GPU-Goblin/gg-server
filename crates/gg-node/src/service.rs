//! The `gg-node` service (ADR-0041, A7): one [`Node`] per control plane where its key is
//! registered, each pulling outputs, stepping runs and renewing its announcements; and the local
//! API the Python library talks to.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use gg_protocol::keys::{PublicKey, SecretKey};
use tokio::sync::RwLock;

use crate::config::ServiceConfig;
use crate::directory::{self, ServerEntry};
use crate::{Node, NodeConfig};

/// A control plane this node works with.
pub struct Served {
    pub entry: ServerEntry,
    pub node: Arc<Node>,
}

/// What the running service shares with its local API.
pub struct Shared {
    pub cfg: ServiceConfig,
    pub served: RwLock<Vec<Served>>,
    /// Servers named by the list where the node is not working yet, with why.
    pub waiting: RwLock<Vec<(ServerEntry, String)>>,
}

/// How often a server where the node could not start is tried again (its key not registered
/// yet, the server down).
const RETRY: Duration = Duration::from_secs(60);

/// The servers to work with: the configured ones, or the signed list.
pub async fn servers(cfg: &ServiceConfig) -> anyhow::Result<Vec<ServerEntry>> {
    if !cfg.servers.is_empty() {
        return Ok(cfg.servers.clone());
    }
    let trusted: Vec<PublicKey> = cfg
        .directory_keys
        .iter()
        .map(|k| gg_protocol::release::parse_ssh_public_key(k))
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("directory_keys: {e}"))?;
    Ok(directory::fetch(
        &cfg.directory_url,
        &trusted,
        &cfg.state_dir,
        cfg.allow_plain_http,
    )
    .await?
    .servers)
}

/// Start the node for one server. The same key everywhere; a store per server.
async fn start_one(cfg: &ServiceConfig, entry: &ServerEntry) -> anyhow::Result<Arc<Node>> {
    let key = SecretKey::load_or_generate(&cfg.key_path).context("node key")?;
    let expect = if entry.server_public_key.is_empty() {
        None
    } else {
        Some((
            entry.network.clone(),
            entry
                .server_public_key
                .parse::<PublicKey>()
                .map_err(|e| anyhow::anyhow!("server key: {e}"))?,
        ))
    };
    let dir: PathBuf = cfg
        .state_dir
        .join("servers")
        .join(if entry.network.is_empty() {
            "default"
        } else {
            entry.network.as_str()
        });
    Node::start(
        NodeConfig {
            server_url: entry.url.clone(),
            allow_plain_http: cfg.allow_plain_http,
            state_dir: dir,
            listen_addr: cfg.p2p_listen.clone(),
            reach: gg_client::reach::ReachConfig {
                advertise_addr: cfg.p2p_advertise.clone(),
                ipv6: cfg.p2p_ipv6,
                nat_pmp: cfg.p2p_nat_pmp,
                upnp: cfg.p2p_upnp,
                gateway: cfg.p2p_gateway.clone(),
                lease_secs: 3600,
            },
            max_conns: cfg.max_conns,
            allow_private_peers: cfg.allow_private_peers,
            give_up_after: Duration::from_secs(cfg.give_up_after_secs),
            expect,
        },
        key,
    )
    .await
}

/// Keep `node` working until `stop`.
fn work(node: &Arc<Node>, stop: &tokio::sync::watch::Receiver<bool>) {
    tokio::spawn(
        node.clone()
            .keep_pulling(Duration::from_millis(20), stop.clone()),
    );
    tokio::spawn(
        node.clone()
            .keep_stepping(Duration::from_secs(1), stop.clone()),
    );
    tokio::spawn(
        node.clone()
            .keep_announcing(Duration::from_secs(60), stop.clone()),
    );
    tokio::spawn(
        node.clone()
            .keep_relaying(Duration::from_secs(1), stop.clone()),
    );
    tokio::spawn(
        node.clone()
            .keep_pushing(Duration::from_secs(1), stop.clone()),
    );
    tokio::spawn(
        node.clone()
            .keep_comparing(Duration::from_secs(1), stop.clone()),
    );
}

/// Run the service until `stop` turns true.
pub async fn run(
    cfg: ServiceConfig,
    stop: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let entries = servers(&cfg).await?;
    let shared = Arc::new(Shared {
        cfg: cfg.clone(),
        served: RwLock::new(Vec::new()),
        waiting: RwLock::new(Vec::new()),
    });
    for entry in entries {
        let shared = shared.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            loop {
                match start_one(&shared.cfg, &entry).await {
                    Ok(node) => {
                        tracing::info!(server = %entry.url, node = %node.id(), "working with this server");
                        work(&node, &stop);
                        shared.waiting.write().await.retain(|(e, _)| e != &entry);
                        shared.served.write().await.push(Served {
                            entry: entry.clone(),
                            node,
                        });
                        return;
                    }
                    Err(e) => {
                        // Most often: the node's key is not registered on this server yet.
                        tracing::warn!(server = %entry.url, error = %format!("{e:#}"), "not working with this server yet");
                        {
                            let mut w = shared.waiting.write().await;
                            w.retain(|(x, _)| x != &entry);
                            w.push((entry.clone(), format!("{e:#}")));
                        }
                        let mut stop = stop.clone();
                        tokio::select! {
                            _ = tokio::time::sleep(RETRY) => {}
                            _ = stop.changed() => { if *stop.borrow() { return; } }
                        }
                    }
                }
            }
        });
    }
    crate::api::serve(shared, stop).await
}
