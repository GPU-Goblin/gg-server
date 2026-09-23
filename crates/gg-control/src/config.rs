//! Server configuration: TOML file with `GG_*` environment overrides.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Server configuration. Durations are integer seconds.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// PostgreSQL URL (`DATABASE_URL` / `GG_DATABASE_URL`).
    pub database_url: String,
    /// Bind address (`GG_BIND_ADDR`).
    pub bind_addr: String,
    /// Data directory for objects and the server key (`GG_DATA_DIR`).
    pub data_dir: PathBuf,
    /// Network identifier (`GG_NETWORK_ID`).
    pub network_id: String,
    /// Pool size.
    pub pool_size: u32,
    /// Initial lease duration for an attempt.
    pub lease_secs: i64,
    /// Hard bound of an attempt lease.
    pub lease_max_secs: i64,
    /// Verification lease duration.
    pub verification_lease_secs: i64,
    /// Time a producer has to send the durable copy of an activation it declared before (ADR-0026);
    /// past it, the staged round that passed the activation on is reissued.
    pub pending_output_secs: i64,
    /// Maximum long-poll wait for claims.
    pub claim_wait_max_secs: u64,
    /// Maximum upload size in bytes.
    pub max_upload_bytes: u64,
    /// Maximum JSON body size in bytes.
    pub max_json_bytes: usize,
    /// Worker/validator session lifetime.
    pub session_ttl_secs: i64,
    /// Login challenge lifetime.
    pub challenge_ttl_secs: i64,
    /// Reaper interval.
    pub reaper_interval_secs: u64,
    /// Development mode: relaxed cookie/TLS assumptions and simulated providers. Refused when
    /// `public` is true.
    pub dev_mode: bool,
    /// Public deployment flag: enforces production-only checks.
    pub public: bool,
    /// Address at which researchers and workers reach this server (behind its HTTPS proxy), used in
    /// what the server writes for them: sign-in links, the CLI command shown with a new token.
    /// https on a public deployment (`GG_PUBLIC_URL`).
    pub public_url: String,
    /// Reverse proxies in front of this server (usually `127.0.0.1` and `::1` for Caddy on the same
    /// machine). Only a request that comes from one of them has its client address read from
    /// `X-Forwarded-For`; from anyone else, the header is ignored. Empty: the connection's address.
    pub trusted_proxies: Vec<std::net::IpAddr>,
    /// Longest an ordinary request may take (seconds).
    pub request_timeout_secs: u64,
    /// Longest an artifact upload may take (seconds): an upload is read whole before the answer,
    /// so this bounds the slowest uplink accepted for a given size (1 GiB in 3600 s ≈ 2.4 Mbit/s).
    pub upload_timeout_secs: u64,
    /// Oldest `gg-worker` version this server gives tasks to (`x.y.z`); empty: no check. Raised
    /// after a release that fixes a correctness or security problem, so that old workers stop and
    /// ask to be updated instead of computing (`GG_MIN_WORKER_VERSION`).
    pub min_worker_version: String,
    /// ORCID / portal settings.
    pub orcid: OrcidConfig,
    /// Sign-in by e-mail link for researchers (a university address, for instance).
    pub email_login: EmailLoginConfig,
    /// Anyone may create a contributor account with an e-mail code, without admission, and attach
    /// machines with its enrollment code (ADR-0030). Needs `[email_login]` to send codes.
    pub contributor_signup: bool,
    /// Sustained request rate per actor (requests per minute).
    pub quota_requests_per_minute: u32,
    /// Burst allowance per actor.
    pub quota_burst: u32,
    /// Multiplier of the quota for worker/validator sessions (they download every artifact of
    /// every task they process).
    pub quota_compute_multiplier: u32,
    /// Peer-assisted distribution (ADR-0012): serve chunk manifests, accept announcements and hand
    /// peer lists to authorized workers. Turning it off leaves HTTP downloads untouched.
    pub p2p_enabled: bool,
    /// Transport chunk size in bytes (manifest granularity).
    pub p2p_chunk_bytes: u32,
    /// Lifetime of a peer announcement.
    pub p2p_peer_ttl_secs: i64,
    /// Lifetime of a peer read ticket.
    pub p2p_ticket_ttl_secs: i64,
    /// Maximum peers returned for one artifact.
    pub p2p_max_peers: i64,
    /// Accept loopback and private announced addresses (single-host demos, laboratory networks).
    /// Always allowed in `dev_mode`.
    pub p2p_allow_private_addrs: bool,
    /// Before advertising an announced address, check that it accepts a TCP connection. A worker
    /// behind a NAT without a usable mapping is then simply not advertised (ADR-0013).
    pub p2p_dial_back: bool,
    /// Timeout of that check, in milliseconds.
    pub p2p_dial_back_timeout_ms: u64,
    /// Identifier of the rule about what data may be submitted (docs/DECISIONS.md, D4). Published by
    /// `/v1/info` and accepted explicitly when a project is created. The default says what this
    /// architecture can honestly support: the contributor sees the data it computes.
    pub data_policy: String,
    /// Share of accepted submissions sent to a second, administratively distinct validator
    /// (ADR-0014). The draw uses a server-side secret and happens after the result is recorded, so a
    /// worker can neither predict nor influence it. No audit is opened while a single operator can
    /// validate. This is the value at start-up: an operator may change it while the server runs
    /// (`POST /v1/admin/config/audit-rate`), because the right rate depends on observed behaviour,
    /// not on a guess made at deployment time (docs/DECISIONS.md, D2).
    pub audit_rate: f64,
    /// Share of the results a GPU validator recomputed on the GPU that a CPU validator recomputes
    /// again (ADR-0031). An engine defect shared by GPU workers and GPU validators passes a
    /// GPU-against-GPU comparison; it is systematic (every task of a given shape), so a sample finds
    /// it within a few dozen tasks. Drawn like an audit, from the server-side secret, after the GPU
    /// decision is recorded; a disagreement opens a dispute for a human.
    pub engine_check_rate: f64,
    /// Memory for the optimizer state kept between rounds (ADR-0036): weights and Adam moments of
    /// each run's trained shards, so a round's step reads only the tasks' gradients and writes the
    /// moments behind the next round. A run whose state does not fit is stepped from disk, every
    /// piece written before the round closes. 0 disables it.
    pub step_cache_bytes: u64,
    /// Remove a collective run's intermediate artifacts once a round is settled (ADR-0040): task
    /// outputs, micro-batch rows, superseded optimizer moments, and the weights of the checkpoints
    /// not kept. Off: everything is kept, and the disk grows by the size of a round every round.
    pub retention: bool,
    /// Seconds after a round closes before its intermediate artifacts may go.
    pub retention_after_secs: u64,
    /// Weights kept every this many steps (with step 0 and the current checkpoint).
    pub checkpoint_keep_every: u32,
    /// Seconds between two retention passes.
    pub retention_interval_secs: u64,
    /// Credits granted to every new project when it is created (0: none, grants are manual). A
    /// research deployment that does not bill its users sets a large amount: researchers submit and
    /// their tasks run when machines are free, while the ledger keeps recording who computed what
    /// (a task still reserves its budget before it starts).
    pub project_initial_grant: u64,
}

/// ORCID provider settings (ADR-0010).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct OrcidConfig {
    /// `disabled` (default: no ORCID sign-in, admin-issued tokens only), `simulated` (dev only),
    /// `sandbox` or `production`.
    pub provider: String,
    /// OAuth client id (public identifier).
    pub client_id: String,
    /// Name of the environment variable holding the client secret (never stored in files).
    pub client_secret_env: String,
    /// Registered redirect URI (must match the ORCID client registration exactly).
    pub redirect_uri: String,
    /// Portal session lifetime.
    pub session_ttl_secs: i64,
    /// OAuth state lifetime.
    pub state_ttl_secs: i64,
}

impl Default for OrcidConfig {
    fn default() -> Self {
        Self {
            provider: "disabled".into(),
            client_id: String::new(),
            client_secret_env: "GG_ORCID_CLIENT_SECRET".into(),
            redirect_uri: "http://127.0.0.1:8080/portal/callback".into(),
            session_ttl_secs: 12 * 3600,
            state_ttl_secs: 600,
        }
    }
}

/// Sign-in by e-mail link. Signing in proves control of an address; admission stays a decision of
/// an operator, exactly as with ORCID.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct EmailLoginConfig {
    /// `disabled` (default), `smtp`, or `simulated` (dev only: messages kept in memory and shown at
    /// `/portal/dev/mailbox`, refused on public deployments).
    pub provider: String,
    /// SMTP relay host.
    pub smtp_host: String,
    /// SMTP port (587 for STARTTLS, 465 for TLS, 25 for a relay on this machine).
    pub smtp_port: u16,
    /// `starttls` (default), `tls`, or `none` (only to a relay on this machine).
    pub smtp_tls: String,
    /// SMTP user name (empty: no authentication).
    pub smtp_username: String,
    /// Name of the environment variable holding the SMTP password (never stored in files).
    pub smtp_password_env: String,
    /// Sender, e.g. `GPU Goblin <noreply@grid.example.org>`.
    pub from: String,
    /// Accepted address domains; a subdomain of an accepted domain is accepted too. Empty: any
    /// domain (admission is manual anyway).
    pub allowed_domains: Vec<String>,
    /// Lifetime of a link.
    pub link_ttl_secs: i64,
    /// Links sent to one address per hour, at most.
    pub max_links_per_hour: i64,
    /// Portal session lifetime after an e-mail sign-in.
    pub session_ttl_secs: i64,
}

impl Default for EmailLoginConfig {
    fn default() -> Self {
        Self {
            provider: "disabled".into(),
            smtp_host: String::new(),
            smtp_port: 587,
            smtp_tls: "starttls".into(),
            smtp_username: String::new(),
            smtp_password_env: "GG_SMTP_PASSWORD".into(),
            from: String::new(),
            allowed_domains: Vec::new(),
            link_ttl_secs: 900,
            max_links_per_hour: 5,
            session_ttl_secs: 12 * 3600,
        }
    }
}

impl EmailLoginConfig {
    fn validate(&self, dev_mode: bool, public: bool) -> Result<(), String> {
        match self.provider.as_str() {
            "disabled" => return Ok(()),
            "simulated" => {
                if !dev_mode || public {
                    return Err("email_login.provider = \"simulated\" requires dev_mode and is refused on public deployments".into());
                }
            }
            "smtp" => {
                if self.smtp_host.trim().is_empty() || self.from.trim().is_empty() {
                    return Err("email_login.smtp_host and email_login.from are required for the smtp provider".into());
                }
                match self.smtp_tls.as_str() {
                    "starttls" | "tls" => {}
                    "none" => {
                        let local =
                            matches!(self.smtp_host.as_str(), "localhost" | "127.0.0.1" | "::1");
                        if !local {
                            return Err("email_login.smtp_tls = \"none\" is only allowed to a relay on this machine (localhost)".into());
                        }
                    }
                    other => return Err(format!("unknown email_login.smtp_tls '{other}'")),
                }
            }
            other => return Err(format!("unknown email_login.provider '{other}'")),
        }
        if !(60..=86_400).contains(&self.link_ttl_secs) {
            return Err("email_login.link_ttl_secs must be between 60 and 86400".into());
        }
        if !(1..=100).contains(&self.max_links_per_hour) {
            return Err("email_login.max_links_per_hour must be between 1 and 100".into());
        }
        for d in &self.allowed_domains {
            let ok = !d.is_empty()
                && d.len() <= 253
                && d.contains('.')
                && d.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-');
            if !ok {
                return Err(format!(
                    "email_login.allowed_domains: '{d}' is not a lowercase domain name"
                ));
            }
        }
        Ok(())
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            database_url: "postgres://gg:gg_dev_password@127.0.0.1:5434/gpu_goblin".into(),
            bind_addr: "127.0.0.1:8080".into(),
            data_dir: PathBuf::from("./var/server"),
            network_id: "gg-dev-local".into(),
            pool_size: 16,
            lease_secs: 120,
            lease_max_secs: 3600,
            verification_lease_secs: 600,
            pending_output_secs: 600,
            claim_wait_max_secs: 30,
            max_upload_bytes: 1 << 30,
            max_json_bytes: 1 << 20,
            session_ttl_secs: 900,
            challenge_ttl_secs: 120,
            reaper_interval_secs: 5,
            dev_mode: false,
            public: false,
            public_url: "http://127.0.0.1:8080".into(),
            min_worker_version: String::new(),
            trusted_proxies: Vec::new(),
            request_timeout_secs: 300,
            upload_timeout_secs: 3600,
            orcid: OrcidConfig::default(),
            email_login: EmailLoginConfig::default(),
            contributor_signup: false,
            quota_requests_per_minute: 600,
            quota_burst: 120,
            quota_compute_multiplier: 10,
            p2p_enabled: true,
            p2p_chunk_bytes: 1 << 20,
            p2p_peer_ttl_secs: 300,
            p2p_ticket_ttl_secs: 300,
            p2p_max_peers: 8,
            p2p_allow_private_addrs: false,
            p2p_dial_back: true,
            p2p_dial_back_timeout_ms: 1500,
            data_policy: "public-synthetic-only".into(),
            audit_rate: 0.25,
            engine_check_rate: 0.1,
            step_cache_bytes: 4 << 30,
            retention: true,
            retention_after_secs: 3600,
            checkpoint_keep_every: 50,
            retention_interval_secs: 300,
            project_initial_grant: 0,
        }
    }
}

impl ServerConfig {
    /// Load from an optional TOML file, then apply environment overrides.
    pub fn load(path: Option<&Path>) -> Result<Self, String> {
        let mut cfg = match path {
            Some(p) => {
                let text =
                    std::fs::read_to_string(p).map_err(|e| format!("read {}: {e}", p.display()))?;
                toml::from_str::<ServerConfig>(&text)
                    .map_err(|e| format!("parse {}: {e}", p.display()))?
            }
            None => ServerConfig::default(),
        };
        cfg.apply_env();
        cfg.validate()?;
        Ok(cfg)
    }

    /// Apply `GG_*` and `DATABASE_URL` overrides.
    pub fn apply_env(&mut self) {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        if let Some(v) = env("GG_DATABASE_URL").or_else(|| env("DATABASE_URL")) {
            self.database_url = v;
        }
        if let Some(v) = env("GG_BIND_ADDR") {
            self.bind_addr = v;
        }
        if let Some(v) = env("GG_PUBLIC_URL") {
            self.public_url = v;
        }
        if let Some(v) = env("GG_MIN_WORKER_VERSION") {
            self.min_worker_version = v;
        }
        if let Some(v) = env("GG_DATA_DIR") {
            self.data_dir = PathBuf::from(v);
        }
        if let Some(v) = env("GG_NETWORK_ID") {
            self.network_id = v;
        }
        if let Some(v) = env("GG_DEV_MODE") {
            self.dev_mode = matches!(v.as_str(), "1" | "true" | "yes");
        }
        if let Some(v) = env("GG_PUBLIC") {
            self.public = matches!(v.as_str(), "1" | "true" | "yes");
        }
        if let Some(v) = env("GG_ORCID_PROVIDER") {
            self.orcid.provider = v;
        }
        if let Some(v) = env("GG_ORCID_CLIENT_ID") {
            self.orcid.client_id = v;
        }
        if let Some(v) = env("GG_ORCID_REDIRECT_URI") {
            self.orcid.redirect_uri = v;
        }
        if let Some(v) = env("GG_PENDING_OUTPUT_SECS").and_then(|v| v.parse().ok()) {
            self.pending_output_secs = v;
        }
        if let Some(v) = env("GG_LEASE_SECS").and_then(|v| v.parse().ok()) {
            self.lease_secs = v;
        }
        if let Some(v) = env("GG_REAPER_INTERVAL_SECS").and_then(|v| v.parse().ok()) {
            self.reaper_interval_secs = v;
        }
        if let Some(v) = env("GG_P2P_ENABLED") {
            self.p2p_enabled = matches!(v.as_str(), "1" | "true" | "yes");
        }
        if let Some(v) = env("GG_P2P_ALLOW_PRIVATE_ADDRS") {
            self.p2p_allow_private_addrs = matches!(v.as_str(), "1" | "true" | "yes");
        }
        if let Some(v) = env("GG_P2P_CHUNK_BYTES").and_then(|v| v.parse().ok()) {
            self.p2p_chunk_bytes = v;
        }
        if let Some(v) = env("GG_AUDIT_RATE").and_then(|v| v.parse().ok()) {
            self.audit_rate = v;
        }
        if let Some(v) = env("GG_ENGINE_CHECK_RATE").and_then(|v| v.parse().ok()) {
            self.engine_check_rate = v;
        }
        if let Some(v) = env("GG_STEP_CACHE_BYTES").and_then(|v| v.parse().ok()) {
            self.step_cache_bytes = v;
        }
        if let Some(v) = env("GG_RETENTION").and_then(|v| v.parse().ok()) {
            self.retention = v;
        }
        if let Some(v) = env("GG_RETENTION_AFTER_SECS").and_then(|v| v.parse().ok()) {
            self.retention_after_secs = v;
        }
        if let Some(v) = env("GG_CHECKPOINT_KEEP_EVERY").and_then(|v| v.parse().ok()) {
            self.checkpoint_keep_every = v;
        }
        if let Some(v) = env("GG_PROJECT_INITIAL_GRANT").and_then(|v| v.parse().ok()) {
            self.project_initial_grant = v;
        }
    }

    /// Validate consistency.
    pub fn validate(&self) -> Result<(), String> {
        gg_core::NetworkId::new(&self.network_id).map_err(|e| e.to_string())?;
        if self.public && self.dev_mode {
            return Err("dev_mode is incompatible with a public deployment".into());
        }
        if self.lease_secs <= 0 || self.lease_max_secs < self.lease_secs {
            return Err("lease_secs must be > 0 and <= lease_max_secs".into());
        }
        if self.claim_wait_max_secs > 120 {
            return Err("claim_wait_max_secs must be <= 120".into());
        }
        match self.orcid.provider.as_str() {
            "disabled" => {}
            "simulated" => {
                if !self.dev_mode || self.public {
                    return Err("orcid.provider = \"simulated\" requires dev_mode and is refused on public deployments".into());
                }
            }
            "sandbox" | "production" => {
                if self.orcid.client_id.is_empty() {
                    return Err(
                        "orcid.client_id is required for the sandbox/production provider".into(),
                    );
                }
                if !self.orcid.redirect_uri.starts_with("https://") && !self.dev_mode {
                    return Err("orcid.redirect_uri must use https outside dev_mode".into());
                }
            }
            other => return Err(format!("unknown orcid.provider '{other}'")),
        }
        if self.public && !self.public_url.starts_with("https://") {
            return Err("public_url must use https on a public deployment (the address behind the HTTPS proxy)".into());
        }
        if self.public_url.len() > 256 || self.public_url.ends_with('/') {
            return Err("public_url must be a base address without a trailing slash".into());
        }
        if !(5..=3600).contains(&self.request_timeout_secs)
            || !(self.request_timeout_secs..=86_400).contains(&self.upload_timeout_secs)
        {
            return Err("request_timeout_secs must be 5..=3600 and upload_timeout_secs between it and 86400".into());
        }
        if !self.min_worker_version.is_empty() && parse_version(&self.min_worker_version).is_none()
        {
            return Err("min_worker_version must be x.y.z".into());
        }
        self.email_login.validate(self.dev_mode, self.public)?;
        if self.contributor_signup && self.email_login.provider == "disabled" {
            return Err("contributor_signup needs [email_login] to send verification codes".into());
        }
        if !(gg_protocol::messages::MIN_CHUNK_BYTES..=gg_protocol::messages::MAX_CHUNK_BYTES)
            .contains(&self.p2p_chunk_bytes)
        {
            return Err(format!(
                "p2p_chunk_bytes must be between {} and {}",
                gg_protocol::messages::MIN_CHUNK_BYTES,
                gg_protocol::messages::MAX_CHUNK_BYTES
            ));
        }
        if self.p2p_peer_ttl_secs <= 0 || self.p2p_ticket_ttl_secs <= 0 {
            return Err("p2p TTLs must be > 0".into());
        }
        if !(1..=64).contains(&self.p2p_max_peers) {
            return Err("p2p_max_peers must be between 1 and 64".into());
        }
        if self.data_policy.trim().is_empty() || self.data_policy.len() > 64 {
            return Err("data_policy must be a short identifier".into());
        }
        if !(0.0..=1.0).contains(&self.audit_rate) || !self.audit_rate.is_finite() {
            return Err("audit_rate must be between 0 and 1".into());
        }
        if !(0.0..=1.0).contains(&self.engine_check_rate) || !self.engine_check_rate.is_finite() {
            return Err("engine_check_rate must be between 0 and 1".into());
        }
        if self.checkpoint_keep_every == 0 {
            return Err("checkpoint_keep_every must be at least 1".into());
        }
        if self.retention_interval_secs == 0 {
            return Err("retention_interval_secs must be at least 1".into());
        }
        if !(50..=10_000).contains(&self.p2p_dial_back_timeout_ms) {
            return Err("p2p_dial_back_timeout_ms must be between 50 and 10000".into());
        }
        if self.public && self.p2p_allow_private_addrs {
            return Err(
                "p2p_allow_private_addrs is refused on a public deployment (peers would be told to connect to private addresses)".into(),
            );
        }
        Ok(())
    }
}

/// Parse `x.y.z` (a pre-release or build suffix after `-` or `+` is ignored for ordering).
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    let version = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(version)
}
