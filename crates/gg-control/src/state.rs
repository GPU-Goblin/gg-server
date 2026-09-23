//! Shared application state.

use std::sync::Arc;

use gg_core::NetworkId;
use gg_protocol::keys::SecretKey;
use gg_storage::{Db, ObjectStore};

use crate::config::ServerConfig;
use crate::mail::Mailer;
use crate::orcid::OrcidProvider;
use crate::quota::{Metrics, Quotas};

/// Application state shared by handlers and background loops.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    config: ServerConfig,
    db: Db,
    objects: ObjectStore,
    network: NetworkId,
    server_key: SecretKey,
    started_at: std::time::Instant,
    orcid: OrcidProvider,
    mailer: Mailer,
    audit_secret: Vec<u8>,
    /// Audit rate in force, adjustable while the server runs. Stored in thousandths so it is an
    /// atomic read on the submission path.
    audit_rate_per_mille: std::sync::atomic::AtomicU32,
    quotas: Quotas,
    metrics: Metrics,
    signals: Signals,
    step_cache: crate::workflows::step_cache::StepCache,
}

/// Wake-ups between the parts of the server. A wait (a worker's or validator's claim, a
/// coordinator pass) wakes up as soon as the event it waits for is recorded, instead of polling
/// the database at a fixed interval. Polling remains as a safety net: a missed wake-up costs only
/// one interval, never a stall.
#[derive(Default)]
pub struct Signals {
    /// Tasks have become claimable (created, requeued).
    pub work: tokio::sync::Notify,
    /// Verifications are available (submission recorded, verification released).
    pub verification: tokio::sync::Notify,
    /// The coordinator has something to examine (submission, decision).
    pub coordinator: tokio::sync::Notify,
}

impl Signals {
    /// Waits for `notify` for at most `max`. The subscription is taken **before** `check`, so that
    /// an event occurring during the check is not lost.
    pub async fn wait_or<T, F, Fut>(
        notify: &tokio::sync::Notify,
        max: std::time::Duration,
        check: F,
    ) -> Result<Option<T>, crate::error::ApiError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Option<T>, crate::error::ApiError>>,
    {
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(v) = check().await? {
            return Ok(Some(v));
        }
        let _ = tokio::time::timeout(max, notified).await;
        Ok(None)
    }
}

impl AppState {
    /// Build the state: opens the object store and loads or creates the server signing key.
    pub async fn new(config: ServerConfig, db: Db) -> Result<Self, String> {
        let objects =
            ObjectStore::open(&config.data_dir.join("objects")).map_err(|e| e.to_string())?;
        let server_key = SecretKey::load_or_generate(&config.data_dir.join("server.key"))
            .map_err(|e| e.to_string())?;
        let network = NetworkId::new(&config.network_id).map_err(|e| e.to_string())?;
        let orcid = OrcidProvider::from_config(&config.orcid)?;
        let mailer = Mailer::from_config(&config.email_login)?;
        if mailer.is_simulated() {
            tracing::warn!("e-mail sign-in is SIMULATED (dev_mode): messages stay in memory");
        }
        // Secret of the audit draw: never leaves the server, never appears in a response or a log.
        let audit_secret = load_or_create_secret(&config.data_dir.join("audit.secret"))?;
        let config_quota = (config.quota_requests_per_minute, config.quota_burst);
        let config_audit_rate = config.audit_rate;
        let config_step_cache = config.step_cache_bytes;
        if orcid.is_simulated() {
            tracing::warn!("ORCID provider is SIMULATED (dev_mode): not a real authentication");
        }
        tracing::info!(network = %network, server_key = %server_key.public_key(), "control plane identity");
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                db,
                objects,
                network,
                server_key,
                started_at: std::time::Instant::now(),
                orcid,
                mailer,
                audit_secret,
                audit_rate_per_mille: std::sync::atomic::AtomicU32::new(
                    (config_audit_rate * 1000.0).round().clamp(0.0, 1000.0) as u32,
                ),
                quotas: Quotas::new(config_quota.0, config_quota.1),
                metrics: Metrics::default(),
                signals: Signals::default(),
                step_cache: crate::workflows::step_cache::StepCache::new(config_step_cache),
            }),
        })
    }

    /// Configuration.
    pub fn config(&self) -> &ServerConfig {
        &self.inner.config
    }
    /// Optimizer state kept between rounds (ADR-0036).
    pub fn step_cache(&self) -> &crate::workflows::step_cache::StepCache {
        &self.inner.step_cache
    }
    /// Database.
    pub fn db(&self) -> &Db {
        &self.inner.db
    }
    /// Object store.
    pub fn objects(&self) -> &ObjectStore {
        &self.inner.objects
    }
    /// Network id.
    pub fn network(&self) -> &NetworkId {
        &self.inner.network
    }
    /// Server signing key.
    pub fn server_key(&self) -> &SecretKey {
        &self.inner.server_key
    }
    /// Quotas.
    pub fn quotas(&self) -> &Quotas {
        &self.inner.quotas
    }
    /// Metrics.
    pub fn metrics(&self) -> &Metrics {
        &self.inner.metrics
    }
    /// Wake-ups between the parts of the server.
    pub fn signals(&self) -> &Signals {
        &self.inner.signals
    }
    /// Tasks have become claimable.
    pub fn signal_work(&self) {
        self.inner.signals.work.notify_waiters();
    }
    /// A submission is recorded: a verification is available and the coordinator must be woken.
    pub fn signal_submission(&self) {
        self.inner.signals.verification.notify_waiters();
        self.inner.signals.coordinator.notify_one();
    }
    /// A decision is recorded: the coordinator must be woken; a rejected task goes back in the queue;
    /// an engine check may have been opened (ADR-0031).
    pub fn signal_decision(&self) {
        self.inner.signals.coordinator.notify_one();
        self.inner.signals.work.notify_waiters();
        self.inner.signals.verification.notify_waiters();
    }
    /// Secret of the audit draw (ADR-0014). Never serialized, never logged.
    pub fn audit_secret(&self) -> &[u8] {
        &self.inner.audit_secret
    }
    /// Audit rate in force right now.
    pub fn audit_rate(&self) -> f64 {
        f64::from(
            self.inner
                .audit_rate_per_mille
                .load(std::sync::atomic::Ordering::Relaxed),
        ) / 1000.0
    }
    /// Change the audit rate while the server runs. Returns the value actually stored (rounded to a
    /// thousandth) or an error when it is out of range.
    pub fn set_audit_rate(&self, rate: f64) -> Result<f64, String> {
        if !rate.is_finite() || !(0.0..=1.0).contains(&rate) {
            return Err("audit_rate must be between 0 and 1".into());
        }
        let per_mille = (rate * 1000.0).round().clamp(0.0, 1000.0) as u32;
        self.inner
            .audit_rate_per_mille
            .store(per_mille, std::sync::atomic::Ordering::Relaxed);
        Ok(f64::from(per_mille) / 1000.0)
    }
    /// ORCID provider.
    pub fn orcid(&self) -> &OrcidProvider {
        &self.inner.orcid
    }

    /// Outgoing e-mail (sign-in links).
    pub fn mailer(&self) -> &Mailer {
        &self.inner.mailer
    }
    /// Uptime in seconds.
    pub fn uptime_secs(&self) -> u64 {
        self.inner.started_at.elapsed().as_secs()
    }
}

/// Load a 32-byte secret from `path`, creating it with mode 0600 on first start.
fn load_or_create_secret(path: &std::path::Path) -> Result<Vec<u8>, String> {
    if let Ok(bytes) = std::fs::read(path)
        && bytes.len() == 32
    {
        return Ok(bytes);
    }
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).map_err(|e| e.to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, secret).map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
    }
    Ok(secret.to_vec())
}
