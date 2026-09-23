//! Per-actor request quotas (token bucket) and bounded request metrics.
//!
//! Keys are the SHA-256 of the bearer token (never the token itself) or, for unauthenticated
//! routes, the client IP. Limits come from configuration; exceeding them yields `429` with
//! `Retry-After`. Metrics use only bounded labels (route template, method, status class).
//!
//! A worker or validator request that its handler finds covered by a live lease of the caller
//! (its own active attempt, or a verification it holds) consumes nothing: that traffic is already
//! bounded by the lease and the task's declared inputs and outputs, and it sits on the critical
//! path of every round. Such a request is still refused while the caller's bucket is empty, so
//! uncovered requests cannot slip through by claiming a lease they do not hold; they are charged
//! once answered.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::{ConnectInfo, MatchedPath, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

/// Token bucket.
#[derive(Clone, Copy, Debug)]
struct Bucket {
    tokens: f64,
    updated: Instant,
}

/// Quota state shared across requests.
#[derive(Debug)]
pub struct Quotas {
    buckets: Mutex<HashMap<String, Bucket>>,
    /// Sustained rate (requests per second).
    pub rate_per_sec: f64,
    /// Burst capacity.
    pub burst: f64,
}

impl Quotas {
    /// New quota table.
    pub fn new(requests_per_minute: u32, burst: u32) -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            rate_per_sec: requests_per_minute as f64 / 60.0,
            burst: burst.max(1) as f64,
        }
    }

    /// Refill `key`'s bucket and admit a request if at least one token is left, without taking it;
    /// returns seconds to wait when refused. The request is charged later with [`Quotas::charge`]
    /// unless its handler found it covered by a lease.
    pub fn admit(&self, key: &str, factor: f64) -> Result<(), u64> {
        let factor = factor.max(1.0);
        let (rate, burst) = (self.rate_per_sec * factor, self.burst * factor);
        let mut map = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let b = Self::refill(&mut map, key, rate, burst);
        if b.tokens >= 1.0 {
            Ok(())
        } else {
            Err(((1.0 - b.tokens) / rate).ceil().max(1.0) as u64)
        }
    }

    /// Take one token for an admitted request. Several requests admitted concurrently can take the
    /// bucket below zero, down to `-burst`: the caller then waits for the debt to refill.
    pub fn charge(&self, key: &str, factor: f64) {
        let factor = factor.max(1.0);
        let (rate, burst) = (self.rate_per_sec * factor, self.burst * factor);
        let mut map = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let b = Self::refill(&mut map, key, rate, burst);
        b.tokens = (b.tokens - 1.0).max(-burst);
    }

    fn refill<'a>(
        map: &'a mut HashMap<String, Bucket>,
        key: &str,
        rate: f64,
        burst: f64,
    ) -> &'a mut Bucket {
        let now = Instant::now();
        if map.len() > 10_000 {
            map.retain(|_, b| now.duration_since(b.updated).as_secs() < 600);
        }
        let b = map.entry(key.to_owned()).or_insert(Bucket {
            tokens: burst,
            updated: now,
        });
        let elapsed = now.duration_since(b.updated).as_secs_f64();
        b.tokens = (b.tokens + elapsed * rate).min(burst);
        b.updated = now;
        b
    }

    /// Try to consume one token for `key`; returns seconds to wait when refused.
    pub fn check(&self, key: &str) -> Result<(), u64> {
        self.check_scaled(key, 1.0)
    }

    /// `check` with the rate and burst multiplied by `factor` (compute-plane sessions download
    /// many artifacts per task; their budget is `quota_compute_multiplier` times the base one).
    pub fn check_scaled(&self, key: &str, factor: f64) -> Result<(), u64> {
        let factor = factor.max(1.0);
        let (rate, burst) = (self.rate_per_sec * factor, self.burst * factor);
        let mut map = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let b = Self::refill(&mut map, key, rate, burst);
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            let wait = ((1.0 - b.tokens) / rate).ceil().max(1.0) as u64;
            Err(wait)
        }
    }
}

/// Response marker: the handler checked that the request is covered by a live lease of the caller.
/// The quota layer then charges nothing for it (see the module documentation).
#[derive(Clone, Copy, Debug)]
pub struct LeaseCovered;

/// Mark a response as covered by a live lease of the caller.
pub fn lease_covered(response: impl IntoResponse) -> Response {
    let mut response = response.into_response();
    response.extensions_mut().insert(LeaseCovered);
    response
}

/// Bounded counters for Prometheus-style export.
#[derive(Debug, Default)]
pub struct Metrics {
    requests: Mutex<HashMap<(String, String, u16), Arc<AtomicU64>>>,
    /// Requests refused by quotas.
    pub quota_rejections: AtomicU64,
    /// Claims that assigned a task.
    pub claims_assigned: AtomicU64,
    /// Decisions applied by outcome index (0 accepted, 1 rejected, 2 inconclusive).
    pub decisions: [AtomicU64; 3],
}

impl Metrics {
    /// Count one request.
    pub fn observe(&self, method: &str, route: &str, status: u16) {
        let class = (status / 100) * 100;
        let mut map = self.requests.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() > 2_000 {
            return; // bounded cardinality
        }
        map.entry((method.to_owned(), route.to_owned(), class))
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Render the request counters in Prometheus text format.
    pub fn render_requests(&self, out: &mut String) {
        let map = self.requests.lock().unwrap_or_else(|e| e.into_inner());
        out.push_str("# TYPE gg_http_requests_total counter\n");
        let mut rows: Vec<_> = map
            .iter()
            .map(|((m, r, c), v)| (m.clone(), r.clone(), *c, v.load(Ordering::Relaxed)))
            .collect();
        rows.sort();
        for (m, r, c, v) in rows {
            out.push_str(&format!(
                "gg_http_requests_total{{method=\"{m}\",route=\"{r}\",status_class=\"{c}\"}} {v}\n"
            ));
        }
        out.push_str(&format!(
            "# TYPE gg_quota_rejections_total counter\ngg_quota_rejections_total {}\n",
            self.quota_rejections.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "# TYPE gg_claims_assigned_total counter\ngg_claims_assigned_total {}\n",
            self.claims_assigned.load(Ordering::Relaxed)
        ));
        out.push_str("# TYPE gg_decisions_total counter\n");
        for (i, name) in ["accepted", "rejected", "inconclusive"].iter().enumerate() {
            out.push_str(&format!(
                "gg_decisions_total{{outcome=\"{name}\"}} {}\n",
                self.decisions[i].load(Ordering::Relaxed)
            ));
        }
    }
}

fn quota_key(req: &Request) -> String {
    if let Some(auth) = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        && let Some(token) = auth.strip_prefix("Bearer ")
    {
        let token = token.trim();
        let class = if token.starts_with(crate::auth::SESSION_TOKEN_PREFIX) {
            "compute"
        } else {
            "token"
        };
        return format!("{class}:{}", crate::auth::token_hash(token).to_hex());
    }
    if let Some(cookie) = req
        .headers()
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        && let Some(sess) = cookie
            .split(';')
            .map(str::trim)
            .find_map(|kv| kv.strip_prefix("gg_session="))
    {
        return format!("session:{}", crate::auth::token_hash(sess).to_hex());
    }
    if let Some(crate::client_ip::ClientIp(ip)) = req.extensions().get() {
        return format!("ip:{ip}");
    }
    match req.extensions().get::<ConnectInfo<SocketAddr>>() {
        Some(ConnectInfo(addr)) => format!("ip:{}", addr.ip()),
        None => "anonymous".to_owned(),
    }
}

/// Middleware: quota check then request metrics.
pub async fn layer(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let method = req.method().to_string();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "unmatched".into());
    let key = quota_key(&req);
    // Worker and validator sessions (`ggs_` bearer tokens) get the compute-plane multiplier, and
    // are charged after the answer, unless it is covered by one of their leases.
    let compute = key.starts_with("compute:");
    let factor = if compute {
        f64::from(state.config().quota_compute_multiplier)
    } else {
        1.0
    };
    let admitted = if compute {
        state.quotas().admit(&key, factor)
    } else {
        state.quotas().check_scaled(&key, factor)
    };
    if let Err(wait) = admitted {
        state
            .metrics()
            .quota_rejections
            .fetch_add(1, Ordering::Relaxed);
        state.metrics().observe(&method, &route, 429);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", wait.to_string())],
            "rate limit exceeded",
        )
            .into_response();
    }
    let resp = next.run(req).await;
    if compute && resp.extensions().get::<LeaseCovered>().is_none() {
        state.quotas().charge(&key, factor);
    }
    state
        .metrics()
        .observe(&method, &route, resp.status().as_u16());
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_refills_at_rate() {
        let q = Quotas::new(60, 3);
        assert!(q.check("a").is_ok() && q.check("a").is_ok() && q.check("a").is_ok());
        assert!(q.check("a").is_err(), "burst exhausted");
        assert!(q.check("b").is_ok(), "independent key");
    }

    #[test]
    fn admitted_requests_are_charged_only_when_told() {
        let q = Quotas::new(60, 2);
        // Covered requests are admitted and never charged: the bucket stays full.
        for _ in 0..100 {
            assert!(q.admit("w", 1.0).is_ok());
        }
        // Uncovered ones are charged after the fact, and refused once the bucket is empty.
        q.charge("w", 1.0);
        q.charge("w", 1.0);
        assert!(q.admit("w", 1.0).is_err(), "bucket exhausted");
        // Concurrent admissions can push it into debt, bounded by the burst.
        for _ in 0..10 {
            q.charge("w", 1.0);
        }
        let wait = q.admit("w", 1.0).unwrap_err();
        assert!(
            (2..=4).contains(&wait),
            "debt bounded by the burst: wait {wait} s"
        );
    }
}
