//! Configuration of the `gg-node` service (TOML; `GG_NODE_*` variables override it).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::directory::ServerEntry;

/// Where the signed list of servers is published by default.
pub const DEFAULT_DIRECTORY_URL: &str = "https://gpugoblin.com/servers.json";

/// Default address of the local API: this machine only.
pub const DEFAULT_API_LISTEN: &str = "127.0.0.1:7450";

/// The service's configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServiceConfig {
    /// The node's key (created by `gg-node init`). One key for every server.
    pub key_path: PathBuf,
    /// State: one store per server, the last server list accepted.
    pub state_dir: PathBuf,
    /// Signed list of servers to work with (`servers.json`, its signature at `.sig`).
    pub directory_url: String,
    /// Keys trusted to sign that list (`ssh-ed25519 AAAA…` lines): the release keys.
    pub directory_keys: Vec<String>,
    /// Servers given here instead of the list (a private deployment, tests). When set, the list
    /// is not fetched.
    pub servers: Vec<ServerEntry>,
    /// Plain `http://` servers accepted (development only).
    pub allow_plain_http: bool,
    /// Local API: where the Python library reaches the node. This machine only by default; any
    /// other address needs `api_tls_cert` and `api_tls_key`.
    pub api_listen: String,
    /// PEM certificate chain of the local API when it listens on the network.
    pub api_tls_cert: Option<PathBuf>,
    /// PEM private key of the local API when it listens on the network.
    pub api_tls_key: Option<PathBuf>,
    /// Largest dataset accepted through the local API, in bytes.
    pub max_data_bytes: u64,
    /// Peer-to-peer: where the chunk server binds (`0.0.0.0:0` picks a port).
    pub p2p_listen: String,
    /// Peer-to-peer: the address announced to the servers (port 0: the bound port). Empty: found
    /// by the node (IPv6, a gateway mapping, the bound or the observed address), each checked by
    /// the server's dial-back.
    pub p2p_advertise: String,
    /// Peer-to-peer: offer a global IPv6 address.
    pub p2p_ipv6: bool,
    /// Peer-to-peer: ask the gateway for a mapping with NAT-PMP.
    pub p2p_nat_pmp: bool,
    /// Peer-to-peer: ask the gateway for a mapping with UPnP-IGD (removed when the node stops).
    pub p2p_upnp: bool,
    /// Peer-to-peer: gateway to ask (`ip:port`); empty: the default route.
    pub p2p_gateway: String,
    /// Peer-to-peer: accept workers at loopback and private addresses (a laboratory network).
    pub allow_private_peers: bool,
    /// Peer-to-peer: connections served at once.
    pub max_conns: u32,
    /// How long pulling an output may keep failing before the node gives up on it.
    pub give_up_after_secs: u64,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
        let base = home.join(".local/share/gpugoblin/node");
        Self {
            key_path: base.join("node.key"),
            state_dir: base.join("state"),
            directory_url: DEFAULT_DIRECTORY_URL.into(),
            directory_keys: Vec::new(),
            servers: Vec::new(),
            allow_plain_http: false,
            api_listen: DEFAULT_API_LISTEN.into(),
            api_tls_cert: None,
            api_tls_key: None,
            max_data_bytes: 64 << 30,
            p2p_listen: "0.0.0.0:0".into(),
            p2p_advertise: String::new(),
            p2p_ipv6: true,
            p2p_nat_pmp: true,
            p2p_upnp: true,
            p2p_gateway: String::new(),
            allow_private_peers: false,
            max_conns: 32,
            give_up_after_secs: 60,
        }
    }
}

impl ServiceConfig {
    /// Load `path` (or the defaults), then the `GG_NODE_*` overrides, and check the result.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let mut cfg = match path {
            Some(p) => toml::from_str::<ServiceConfig>(
                &std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?,
            )
            .with_context(|| format!("parse {}", p.display()))?,
            None => ServiceConfig::default(),
        };
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        if let Some(v) = env("GG_NODE_KEY") {
            cfg.key_path = PathBuf::from(v);
        }
        if let Some(v) = env("GG_NODE_STATE_DIR") {
            cfg.state_dir = PathBuf::from(v);
        }
        if let Some(v) = env("GG_NODE_DIRECTORY_URL") {
            cfg.directory_url = v;
        }
        if let Some(v) = env("GG_NODE_API_LISTEN") {
            cfg.api_listen = v;
        }
        cfg.validate()?;
        Ok(cfg)
    }

    /// Refuse what cannot work, or would be unsafe.
    pub fn validate(&self) -> anyhow::Result<()> {
        let api: SocketAddr = self
            .api_listen
            .parse()
            .with_context(|| format!("api_listen '{}' is not ip:port", self.api_listen))?;
        let tls = self.api_tls_cert.is_some() && self.api_tls_key.is_some();
        if !api.ip().is_loopback() && !tls {
            bail!(
                "api_listen {api} is reachable from the network: researchers' tokens would travel \
                 in clear; set api_tls_cert and api_tls_key, or listen on 127.0.0.1"
            );
        }
        if self.api_tls_cert.is_some() != self.api_tls_key.is_some() {
            bail!("api_tls_cert and api_tls_key go together");
        }
        if self.servers.is_empty() && self.directory_keys.is_empty() {
            bail!(
                "no server: give `servers`, or `directory_keys` (the release keys that sign the \
                 list at directory_url)"
            );
        }
        for k in &self.directory_keys {
            gg_protocol::release::parse_ssh_public_key(k)
                .map_err(|e| anyhow::anyhow!("directory_keys: {e}"))?;
        }
        Ok(())
    }

    /// Whether the local API listens beyond this machine.
    pub fn api_on_network(&self) -> bool {
        self.api_listen
            .parse::<SocketAddr>()
            .is_ok_and(|a| !a.ip().is_loopback())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_server() -> ServiceConfig {
        ServiceConfig {
            servers: vec![ServerEntry {
                name: String::new(),
                url: "https://grid.gpugoblin.com".into(),
                network: "gg-beta".into(),
                server_public_key: "00".repeat(32),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn the_local_api_needs_tls_to_listen_on_the_network() {
        assert!(with_server().validate().is_ok());
        let mut open = with_server();
        open.api_listen = "0.0.0.0:7450".into();
        assert!(
            open.validate()
                .unwrap_err()
                .to_string()
                .contains("in clear")
        );
        open.api_tls_cert = Some("cert.pem".into());
        assert!(open.validate().is_err(), "a certificate without its key");
        open.api_tls_key = Some("key.pem".into());
        assert!(open.validate().is_ok());
        assert!(open.api_on_network());
    }

    #[test]
    fn a_node_needs_servers_or_keys_to_trust_the_list() {
        let none = ServiceConfig::default();
        assert!(
            none.validate()
                .unwrap_err()
                .to_string()
                .contains("no server")
        );
        let bad = ServiceConfig {
            directory_keys: vec!["ssh-rsa AAAA".into()],
            ..Default::default()
        };
        assert!(bad.validate().is_err());
    }
}
