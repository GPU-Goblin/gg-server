//! Administered validator: claims verification work, recomputes the reference (on the CPU
//! profile, or for model tasks on a GPU, ADR-0031), compares within the task's tolerance and posts
//! a signed decision stating the profile it recomputed with.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gg_client::client::{Api, ClientError, ServerInfo};
use gg_core::*;
use gg_protocol::domain::Domain;
use gg_protocol::keys::SecretKey;
use gg_protocol::messages::{ResultSubmissionV1, TaskSpecV1, VerificationDecisionV1};
use gg_protocol::signed::Signed;
use serde::{Deserialize, Serialize};

/// Validator configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifierConfig {
    /// Control plane URL.
    pub server_url: String,
    /// Key file.
    pub key_path: PathBuf,
    /// Working directory.
    pub work_dir: PathBuf,
    /// Long-poll wait.
    pub claim_wait_secs: u64,
    /// Maximum artifact size accepted.
    pub max_download_bytes: u64,
    /// Local input cache budget in `work_dir/cache` (bytes, 0 disables); entries are re-hashed on
    /// every hit (`gg_client::cache`), the cache is never a source of authority.
    pub cache_max_bytes: u64,
    /// Engine the references of model tasks are recomputed with: `cpu` (the reference profile,
    /// default) or `gpu` / `gpu:<index>` (a build with the `gpu` feature, ADR-0031). Every other
    /// kind is recomputed on the CPU. A GPU validator is never offered engine checks, and a sample
    /// of what it accepts is recomputed again by a CPU validator. The GPU is selected explicitly
    /// and never falls back to the CPU.
    pub engine: String,
    /// Take only secondary verifications, audits and engine checks, never a primary one (a CPU
    /// validator next to GPU validators, ADR-0031): the GPU ones verify quickly, this one
    /// recomputes on the CPU the samples the server draws.
    pub secondary_only: bool,
    /// Download the inputs of tasks being computed now, which this validator will soon verify,
    /// into the cache (ADR-0037): a round's checkpoint is then already here when the submission
    /// arrives, instead of being fetched on the critical path.
    pub prefetch: bool,
    /// Accept loopback and private addresses for the peers an artifact is read from (a laboratory
    /// network, single-host demonstrations). What a lab node holds is read through peers
    /// (ADR-0041); the control plane has no copy of it.
    pub allow_private_peers: bool,
    /// Listen here (`ip:port`) for artifacts pushed by holders that cannot be reached, such as a
    /// lab node behind a NAT (ADR-0041, A8). Empty (the default): no listening socket, and such
    /// artifacts cannot be verified here. `push_advertise`: the address to give instead of the ones
    /// found (IPv6, gateway mapping, bound, observed).
    pub push_listen: String,
    pub push_advertise: String,
    /// Public deployment flag: refuses to start with development faults enabled.
    pub public: bool,
    /// Controlled fault injection for demonstrations (never remotely activatable).
    pub dev_faults: VerifierFaults,
}

/// Local fault injection of the validator, for the federated-validation demonstration.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct VerifierFaults {
    /// Accept every submission without recomputing anything: a validator that vouches for whatever
    /// it is given. Its disagreement with an honest validator must open a dispute (ADR-0014).
    pub always_accept: bool,
}

impl VerifierFaults {
    /// Any fault enabled.
    pub fn any(&self) -> bool {
        self.always_accept
    }
}

impl Default for VerifierConfig {
    fn default() -> Self {
        Self {
            server_url: "http://127.0.0.1:8080".into(),
            key_path: PathBuf::from("./var/verifier/verifier.key"),
            work_dir: PathBuf::from("./var/verifier/work"),
            claim_wait_secs: 20,
            max_download_bytes: 2 << 30,
            cache_max_bytes: 4 << 30,
            engine: "cpu".into(),
            secondary_only: false,
            prefetch: true,
            allow_private_peers: false,
            push_listen: String::new(),
            push_advertise: String::new(),
            public: false,
            dev_faults: VerifierFaults::default(),
        }
    }
}

impl VerifierConfig {
    /// Load with environment overrides.
    pub fn load(path: Option<&std::path::Path>) -> anyhow::Result<Self> {
        let mut cfg = match path {
            Some(p) => toml::from_str::<VerifierConfig>(&std::fs::read_to_string(p)?)?,
            None => VerifierConfig::default(),
        };
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        if let Some(v) = env("GG_SERVER_URL") {
            cfg.server_url = v;
        }
        if let Some(v) = env("GG_VERIFIER_KEY") {
            cfg.key_path = PathBuf::from(v);
        }
        if let Some(v) = env("GG_VERIFIER_WORK_DIR") {
            cfg.work_dir = PathBuf::from(v);
        }
        if let Some(v) = env("GG_VERIFIER_CLAIM_WAIT_SECS").and_then(|v| v.parse().ok()) {
            cfg.claim_wait_secs = v;
        }
        if let Some(v) = env("GG_VERIFIER_ENGINE") {
            cfg.engine = v;
        }
        if let Some(v) = env("GG_VERIFIER_ALLOW_PRIVATE_PEERS") {
            cfg.allow_private_peers = v == "1" || v == "true";
        }
        let engine = cfg.engine()?;
        match engine {
            gg_compute::engine::EngineSelection::FlexCpu => {}
            gg_compute::engine::EngineSelection::WgpuVulkan { .. } => anyhow::ensure!(
                cfg!(feature = "gpu"),
                "engine '{}' needs a build with the `gpu` feature",
                cfg.engine
            ),
            gg_compute::engine::EngineSelection::TorchCuda { .. } => anyhow::ensure!(
                cfg!(feature = "cuda"),
                "engine '{}' needs a build with the `cuda` feature",
                cfg.engine
            ),
        }
        anyhow::ensure!(cfg.claim_wait_secs <= 120, "claim_wait_secs must be <= 120");
        anyhow::ensure!(
            !(cfg.public && cfg.dev_faults.any()),
            "dev_faults are incompatible with public mode"
        );
        gg_client::client::check_transport(&cfg.server_url, !cfg.public)
            .map_err(|e| anyhow::anyhow!("server_url: {e}"))?;
        Ok(cfg)
    }
}

impl VerifierConfig {
    /// The engine selected by `engine`.
    pub fn engine(&self) -> anyhow::Result<gg_compute::engine::EngineSelection> {
        gg_compute::engine::EngineSelection::parse(&self.engine)
            .map_err(|e| anyhow::anyhow!("engine: {e}"))
    }
}

/// Verification work as served by the control plane.
#[derive(Clone, Debug, Deserialize)]
pub struct Work {
    /// Verification id.
    pub verification_id: uuid::Uuid,
    /// Task.
    pub task_id: TaskId,
    /// Attempt.
    pub attempt_id: AttemptId,
    /// Spec.
    pub spec: TaskSpecV1,
    /// Submission.
    pub submission: Signed<ResultSubmissionV1>,
}

/// Outcome recorded by the validator loop.
#[derive(Clone, Debug)]
pub struct Decided {
    /// Task.
    pub task_id: TaskId,
    /// Attempt.
    pub attempt_id: AttemptId,
    /// Outcome.
    pub outcome: VerificationOutcome,
    /// Server response.
    pub response: serde_json::Value,
}

/// The validator.
pub struct Verifier {
    cfg: VerifierConfig,
    engine: gg_compute::engine::EngineSelection,
    api: Api,
    key: std::sync::Arc<SecretKey>,
    info: ServerInfo,
    /// Downloads, from the control plane or, for what it does not store, from peers.
    reader: Reader,
    validator_id: Option<ValidatorId>,
    token: Option<(String, Instant)>,
    cache: gg_client::cache::ArtifactCache,
    /// One download at a time per artifact, shared with the prefetcher.
    locks: FetchLocks,
    /// The session token, shared with the prefetcher.
    shared_token: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Decisions made.
    pub decided: Vec<Decided>,
}

/// One download at a time per artifact, shared by the prefetcher and verifications: a
/// verification that needs an artifact being prefetched waits for it instead of downloading it a
/// second time.
#[derive(Clone, Default)]
struct FetchLocks(
    std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<Sha256Digest, std::sync::Arc<tokio::sync::Mutex<()>>>,
        >,
    >,
);

impl FetchLocks {
    fn of(&self, hash: &Sha256Digest) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        // Locks nobody holds or waits for are dropped.
        map.retain(|_, l| std::sync::Arc::strong_count(l) > 1);
        map.entry(*hash).or_default().clone()
    }
}

/// Reads an artifact from the control plane, or through peers when it has no copy (`409`: an
/// artifact a lab node holds, ADR-0041). Peer chunks are checked against the manifest the server
/// signs, and the whole against its hash, exactly as a worker does.
/// Why a read failed: the control plane's copy, or the peers holding the only one. A worker is
/// never judged on the second (the holder may simply be offline): the verification is released
/// and tried again.
#[derive(Debug)]
enum ReadError {
    Server(ClientError),
    Peers(ClientError),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Server(e) => write!(f, "{e}"),
            Self::Peers(e) => write!(f, "through peers: {e}"),
        }
    }
}

impl std::error::Error for ReadError {}

#[derive(Clone)]
struct Reader {
    api: Api,
    key: std::sync::Arc<SecretKey>,
    network: gg_core::NetworkId,
    server_key: gg_protocol::keys::PublicKey,
    max: u64,
    peers: bool,
    allow_private_peers: bool,
    /// Where holders push what nobody can fetch from them (A8), when configured.
    push: Option<std::sync::Arc<(gg_client::p2p::PeerServer, gg_client::reach::Reach)>>,
}

impl Reader {
    async fn nobody_reachable_holds(
        &self,
        token: &str,
        r: &gg_protocol::messages::ArtifactRef,
    ) -> bool {
        let Ok(peers) = self
            .api
            .get_json::<serde_json::Value>(token, &format!("/validator/artifacts/{}/peers", r.hash))
            .await
        else {
            return false;
        };
        peers["peers"].as_array().is_none_or(|p| p.is_empty())
            && peers["server_holds"] == serde_json::json!(false)
    }

    async fn receive_by_push(
        &self,
        token: &str,
        r: &gg_protocol::messages::ArtifactRef,
        dest: &std::path::Path,
        push: &(gg_client::p2p::PeerServer, gg_client::reach::Reach),
    ) -> Result<(), ReadError> {
        let (server, reach) = push;
        let mut addrs: Vec<String> = reach.current().await.map(|(a, _)| a).into_iter().collect();
        for c in reach.candidates(server.addr(), async { None }).await {
            if !addrs.contains(&c.addr) {
                addrs.push(c.addr);
            }
        }
        gg_client::p2p::fetch_by_push(
            &self.api,
            token,
            "/validator",
            &self.network,
            &self.server_key,
            r,
            dest,
            server,
            &addrs,
            std::time::Duration::from_secs(60),
        )
        .await
        .map_err(ReadError::Peers)?;
        tracing::info!(hash = %r.hash, "received by push");
        Ok(())
    }

    async fn download(
        &self,
        token: &str,
        r: &gg_protocol::messages::ArtifactRef,
        dest: &std::path::Path,
    ) -> Result<(), ReadError> {
        let path = format!("/validator/artifacts/{}", r.hash);
        match self
            .api
            .download_retry(token, &path, r, self.max, dest)
            .await
        {
            Err(ClientError::Status { status: 409, .. }) if self.peers => {
                if r.size > self.max {
                    return Err(ReadError::Server(ClientError::Integrity(format!(
                        "artifact of {} bytes above the local limit",
                        r.size
                    ))));
                }
                if let Some(push) = &self.push
                    && self.nobody_reachable_holds(token, r).await
                {
                    return self.receive_by_push(token, r, dest, push).await;
                }
                let stats = gg_client::p2p::fetch_with_peers(
                    &self.api,
                    token,
                    &self.key,
                    &self.network,
                    &self.server_key,
                    r,
                    &path,
                    dest,
                    &gg_client::p2p::FetchConfig {
                        allow_private_peers: self.allow_private_peers,
                        tracker: "/validator".into(),
                        ..Default::default()
                    },
                )
                .await
                .map_err(ReadError::Peers)?;
                tracing::info!(hash = %r.hash, from_peers = stats.chunks_from_peers, "read through peers");
                Ok(())
            }
            other => other.map_err(ReadError::Server),
        }
    }
}

/// Inputs announced by the server for prefetching.
#[derive(Deserialize)]
struct Upcoming {
    artifacts: Vec<gg_protocol::messages::ArtifactRef>,
}

/// How often the prefetcher asks what is being computed.
const PREFETCH_EVERY: Duration = Duration::from_millis(500);

/// An artifact from the cache when an intact copy is there, downloaded (and cached) otherwise,
/// under its lock. Returns whether it came from the cache.
async fn fetch_cached(
    reader: &Reader,
    cache: &gg_client::cache::ArtifactCache,
    locks: &FetchLocks,
    token: &str,
    r: &gg_protocol::messages::ArtifactRef,
    dest: &std::path::Path,
) -> anyhow::Result<bool> {
    let lock = locks.of(&r.hash);
    let _held = lock.lock().await;
    if cache.fetch(&r.hash, r.size, dest).ok() == Some(gg_client::cache::Lookup::Hit) {
        return Ok(true);
    }
    reader.download(token, r, dest).await?;
    if let Err(e) = cache.store(&r.hash, dest) {
        tracing::warn!(error = %e, hash = %r.hash, "artifact cache store failed");
    }
    Ok(false)
}

/// Downloads into the cache the inputs of tasks being computed now (ADR-0037), until shutdown.
/// Only what the cache can hold; what is already there is not fetched again (every use re-hashes
/// it anyway).
#[allow(clippy::too_many_arguments)]
async fn prefetch(
    api: Api,
    reader: Reader,
    cache: gg_client::cache::ArtifactCache,
    locks: FetchLocks,
    token: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    profile: String,
    dir: PathBuf,
    (max, cache_max): (u64, u64),
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, "no prefetch directory, prefetching disabled");
        return;
    }
    let body = serde_json::json!({ "reference_profile": profile });
    loop {
        tokio::select! {
            _ = tokio::time::sleep(PREFETCH_EVERY) => {}
            _ = shutdown.changed() => return,
        }
        let Some(token) = token.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            continue;
        };
        let upcoming = match api
            .post_json::<Upcoming>(&token, "/validator/upcoming", &body)
            .await
        {
            Ok(Some(u)) => u.artifacts,
            Ok(None) => continue,
            Err(e) => {
                tracing::debug!(error = %e, "prefetch: upcoming inputs unavailable");
                continue;
            }
        };
        for r in upcoming {
            if r.size > max || r.size > cache_max / 2 || cache.path_of(&r.hash).exists() {
                continue;
            }
            let lock = locks.of(&r.hash);
            let _held = lock.lock().await;
            if cache.path_of(&r.hash).exists() {
                continue;
            }
            let dest = dir.join(r.hash.to_hex());
            let started = Instant::now();
            match reader.download(&token, &r, &dest).await {
                Ok(()) => match cache.store(&r.hash, &dest) {
                    Ok(()) => {
                        tracing::info!(hash = %r.hash, bytes = r.size, ms = started.elapsed().as_millis() as u64, "input prefetched")
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, hash = %r.hash, "prefetch: cache store failed")
                    }
                },
                Err(e) => tracing::debug!(error = %e, hash = %r.hash, "prefetch: download failed"),
            }
            let _ = std::fs::remove_file(&dest);
        }
    }
}

impl Verifier {
    /// Load key and discover the server.
    pub async fn new(cfg: VerifierConfig) -> anyhow::Result<Self> {
        let key = SecretKey::load_or_generate(&cfg.key_path)?;
        let api = if cfg.public {
            Api::new(&cfg.server_url)?
        } else {
            Api::new_allowing_plain_http(&cfg.server_url)?
        };
        let info = api.info().await?;
        anyhow::ensure!(
            info.protocol_version == gg_protocol::PROTOCOL_VERSION,
            "server protocol version {} unsupported",
            info.protocol_version
        );
        std::fs::create_dir_all(&cfg.work_dir)?;
        let engine = cfg.engine()?;
        #[cfg(feature = "gpu")]
        if let gg_compute::engine::EngineSelection::WgpuVulkan { device_index } = engine {
            // Explicit failure, never a fallback to the CPU; initialized once for the process.
            let (_, info) = gg_compute::engine::gpu::init_vulkan(device_index)
                .map_err(|e| anyhow::anyhow!("GPU engine: {e}"))?;
            tracing::info!(device = %info.device_name, profile = info.numeric_profile, "model tasks recomputed on the GPU");
        }
        #[cfg(feature = "cuda")]
        if let gg_compute::engine::EngineSelection::TorchCuda { device_index } = engine {
            // Strict float32 is checked here, before any verification: the variable must be set
            // by whoever starts the validator (the systemd unit sets it).
            let info = gg_compute::engine::cuda::check(device_index)
                .map_err(|e| anyhow::anyhow!("CUDA engine: {e}"))?;
            tracing::info!(device = %info.device_name, profile = info.numeric_profile, "model tasks recomputed on the native CUDA engine");
        }
        let cache =
            gg_client::cache::ArtifactCache::new(cfg.work_dir.join("cache"), cfg.cache_max_bytes);
        let key = std::sync::Arc::new(key);
        // A chunk server only to receive pushes (A8), when configured: it serves nothing a ticket
        // does not allow, and takes nothing it does not wait for.
        let push = if cfg.push_listen.is_empty() || info.p2p_chunk_bytes == 0 {
            None
        } else {
            let server = gg_client::p2p::PeerServer::start(
                &cfg.push_listen,
                cache.clone(),
                info.network_id.clone(),
                info.server_public_key,
                8,
                0,
            )
            .await?;
            let reach = gg_client::reach::Reach::new(gg_client::reach::ReachConfig {
                advertise_addr: cfg.push_advertise.clone(),
                ..Default::default()
            });
            Some(std::sync::Arc::new((server, reach)))
        };
        let reader = Reader {
            api: api.clone(),
            key: key.clone(),
            network: info.network_id.clone(),
            server_key: info.server_public_key,
            max: cfg.max_download_bytes,
            peers: info.p2p_chunk_bytes > 0,
            allow_private_peers: cfg.allow_private_peers,
            push,
        };
        Ok(Self {
            cfg,
            engine,
            api,
            key,
            info,
            reader,
            validator_id: None,
            token: None,
            cache,
            locks: FetchLocks::default(),
            shared_token: Default::default(),
            decided: Vec::new(),
        })
    }

    /// Public key (to be authorized by an operator).
    pub fn public_key(&self) -> gg_protocol::keys::PublicKey {
        self.key.public_key()
    }

    async fn session(&mut self) -> Result<(String, ValidatorId), ClientError> {
        if let (Some((t, exp)), Some(id)) = (&self.token, self.validator_id)
            && Instant::now() + Duration::from_secs(60) < *exp
        {
            return Ok((t.clone(), id));
        }
        let s = self
            .api
            .login(&self.key, &self.info.network_id, "validator-login")
            .await?;
        let id = ValidatorId(s.principal_id);
        self.validator_id = Some(id);
        *self.shared_token.lock().unwrap_or_else(|e| e.into_inner()) = Some(s.token.clone());
        self.token = Some((
            s.token.clone(),
            Instant::now() + Duration::from_secs(s.expires_in_secs.max(90) as u64),
        ));
        Ok((s.token, id))
    }

    /// Verify work items until shutdown (or `max` items).
    pub async fn run(
        &mut self,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
        max: Option<usize>,
    ) -> anyhow::Result<()> {
        let mut done = 0usize;
        let prefetcher = self.cfg.prefetch.then(|| {
            tokio::spawn(prefetch(
                self.api.clone(),
                self.reader.clone(),
                self.cache.clone(),
                self.locks.clone(),
                self.shared_token.clone(),
                self.engine.numeric_profile().to_owned(),
                self.cfg.work_dir.join("prefetch"),
                (self.cfg.max_download_bytes, self.cfg.cache_max_bytes),
                shutdown.clone(),
            ))
        });
        // The prefetcher ends with the loop, whatever way it ends.
        let _stop = prefetcher.map(AbortOnDrop);
        loop {
            if *shutdown.borrow() || max.is_some_and(|m| done >= m) {
                return Ok(());
            }
            let (token, validator_id) = match self.session().await {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(error = %e, "validator login failed (is the key authorized? `gg admin validator add`)");
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                        _ = shutdown.changed() => {}
                    }
                    continue;
                }
            };
            let body = serde_json::json!({
                "wait_secs": self.cfg.claim_wait_secs,
                "reference_profile": self.engine.numeric_profile(),
                "secondary_only": self.cfg.secondary_only,
            });
            let work = tokio::select! {
                r = self.api.post_json::<Work>(&token, "/validator/claim", &body) => r,
                _ = shutdown.changed() => { continue; }
            };
            match work {
                Ok(Some(w)) => {
                    done += 1;
                    let verification_id = w.verification_id;
                    match self.verify(&token, validator_id, w).await {
                        Ok(d) => {
                            tracing::info!(task_id = %d.task_id, attempt_id = %d.attempt_id, outcome = %d.outcome, applied = %d.response["applied"], "decision posted");
                            self.decided.push(d);
                        }
                        Err(e) => {
                            // Give the work item back at once instead of holding it until the
                            // lease expires (another validator, or this one, will retry).
                            tracing::warn!(error = %e, %verification_id, "verification failed, releasing it");
                            if let Err(e) = self
                                .api
                                .post_json::<serde_json::Value>(
                                    &token,
                                    &format!("/validator/verifications/{verification_id}/release"),
                                    &serde_json::json!({}),
                                )
                                .await
                            {
                                tracing::warn!(error = %e, "release failed (the lease will expire)");
                            }
                        }
                    }
                }
                Ok(None) => {}
                Err(ClientError::Status { status: 401, .. }) => self.token = None,
                Err(e) => {
                    tracing::warn!(error = %e, "claim failed, backing off");
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(3)) => {}
                        _ = shutdown.changed() => {}
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn verify(
        &mut self,
        token: &str,
        validator_id: ValidatorId,
        w: Work,
    ) -> anyhow::Result<Decided> {
        w.submission.verify(&self.info.network_id, Domain::Result)?;
        let spec_hash =
            gg_protocol::domain::hash_message(&self.info.network_id, Domain::TaskSpec, &w.spec)?;
        anyhow::ensure!(
            spec_hash == w.submission.body.spec_hash,
            "submission references another spec"
        );
        let started = std::time::Instant::now();
        let task = gg_compute::validate_definition(&w.spec.definition)?;
        let dir = self.cfg.work_dir.join(w.verification_id.to_string());
        std::fs::create_dir_all(&dir)?;
        // Inputs (checkpoints, datasets) are shared by many tasks, and prefetched while the worker
        // computes (ADR-0037): served from the local cache when an intact copy (re-hashed) exists,
        // downloaded otherwise. Inputs and outputs are fetched at once.
        let (reader, cache, locks) = (&self.reader, &self.cache, &self.locks);
        let dir_ref = &dir;
        let inputs_f = futures_util::future::try_join_all(w.spec.definition.inputs.iter().map(
            |(name, r)| async move {
                let _timed = TimedInput(name.clone(), r.size, std::time::Instant::now());
                let dest = dir_ref.join(format!("in-{name}"));
                let hit = fetch_cached(reader, cache, locks, token, r, &dest).await?;
                Ok::<_, anyhow::Error>((name.clone(), dest, hit))
            },
        ));
        // An output the control plane cannot serve intact yields a decision (missing candidate →
        // inconclusive/rejected by the catalog verifier), never a crash of the validator. One held
        // only by peers that cannot be read now (a lab node offline) is not the worker's fault:
        // the verification is released and tried again.
        let outputs_f = futures_util::future::join_all(w.submission.body.outputs.iter().map(
            |(name, r)| async move {
                let dest = dir_ref.join(format!("out-{name}"));
                match reader.download(token, r, &dest).await {
                    Ok(()) => Ok(Some((name.clone(), dest))),
                    Err(ReadError::Server(e)) => {
                        tracing::warn!(error = %e, output = %name, "output download failed");
                        Ok(None)
                    }
                    Err(e @ ReadError::Peers(_)) => Err(e),
                }
            },
        ));
        let (fetched_inputs, fetched_outputs) = tokio::join!(inputs_f, outputs_f);
        let fetched_inputs = fetched_inputs?;
        let cache_hits = fetched_inputs.iter().filter(|(_, _, hit)| *hit).count();
        let inputs: BTreeMap<String, PathBuf> = fetched_inputs
            .into_iter()
            .map(|(name, dest, _)| (name, dest))
            .collect();
        let mut outputs = BTreeMap::<String, PathBuf>::new();
        for o in fetched_outputs {
            if let Some((name, dest)) = o? {
                outputs.insert(name, dest);
            }
        }
        let fetched = started.elapsed();
        let (mut verdict, reference_profile) = gg_compute::verify_task_on(
            &task,
            &inputs,
            &outputs,
            &w.submission.body.outputs,
            &w.submission.body.execution_profile.numeric_profile,
            self.engine,
        )?;
        if self.cfg.dev_faults.always_accept {
            tracing::warn!(task_id = %w.task_id, "DEV FAULT: accepting without trusting the recomputation");
            verdict.outcome = gg_core::VerificationOutcome::Accepted;
            verdict.checks = vec![gg_protocol::messages::CheckV1 {
                name: "dev-fault.vouched".into(),
                passed: true,
                measured: None,
                tolerance: None,
                detail: "accepted by a validator running with always_accept".into(),
            }];
        }
        let computed = started.elapsed();
        let decision = VerificationDecisionV1 {
            protocol_version: gg_protocol::PROTOCOL_VERSION,
            network: self.info.network_id.clone(),
            decision_id: DecisionId::new(),
            submission_hash: w
                .submission
                .body_hash(&self.info.network_id, Domain::Result)?,
            task_id: w.task_id,
            attempt_id: w.attempt_id,
            validator_id,
            method: "recompute-reference".into(),
            method_version: w.spec.definition.validation_policy.method_version,
            reference_profile,
            checks: verdict.checks,
            artifacts_checked: verdict.artifacts_checked,
            outcome: verdict.outcome,
            decided_at: UnixMillis::now(),
        };
        let signed = Signed::sign(
            &self.key,
            &self.info.network_id,
            Domain::Verification,
            decision,
        )?;
        let response: serde_json::Value = self
            .api
            .post_json(
                token,
                &format!("/validator/verifications/{}/decision", w.verification_id),
                &serde_json::to_value(&signed)?,
            )
            .await?
            .unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        // Where a verification's time goes: inputs and outputs, recomputation, posting the decision.
        tracing::info!(
            fetch_ms = fetched.as_millis() as u64,
            cache_hits,
            compute_ms = (computed - fetched).as_millis() as u64,
            post_ms = (started.elapsed() - computed).as_millis() as u64,
            "verification timed"
        );
        Ok(Decided {
            task_id: w.task_id,
            attempt_id: w.attempt_id,
            outcome: verdict.outcome,
            response,
        })
    }
}

/// Aborts a spawned task when dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Logs how long an input took to fetch, at the end of its loop iteration.
struct TimedInput(String, u64, std::time::Instant);

impl Drop for TimedInput {
    fn drop(&mut self) {
        tracing::debug!(input = %self.0, bytes = self.1, ms = self.2.elapsed().as_millis() as u64, "input fetched");
    }
}

#[cfg(test)]
mod tests {
    use super::VerifierConfig;

    #[test]
    fn a_public_validator_reaches_its_server_only_over_https() {
        let dir = std::env::temp_dir().join(format!("gg-verifier-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let load = |url: &str, public: bool| {
            let path = dir.join("verifier.toml");
            std::fs::write(
                &path,
                format!("server_url = \"{url}\"\npublic = {public}\n"),
            )
            .unwrap();
            VerifierConfig::load(Some(&path)).map_err(|e| e.to_string())
        };
        let e = load("http://grid.example.org", true).unwrap_err();
        assert!(e.contains("server_url") && e.contains("use https"), "{e}");
        assert!(load("https://grid.example.org", true).is_ok());
        assert!(load("http://127.0.0.1:8080", true).is_ok());
        assert!(load("http://192.168.1.10:8080", false).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }
}
