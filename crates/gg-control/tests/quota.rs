//! Per-actor quotas and the metrics endpoint.

use gg_control::config::ServerConfig;
use gg_control::server::serve;
use gg_control::state::AppState;
use gg_storage::queries::identities;
use gg_storage::testing::TestDb;

#[tokio::test]
async fn quota_refuses_bursts_and_metrics_report_them() {
    let tdb = TestDb::create_or_panic().await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = ServerConfig {
        database_url: tdb.url(),
        data_dir: tmp.path().to_path_buf(),
        network_id: "gg-quota".into(),
        quota_requests_per_minute: 60,
        quota_burst: 5,
        ..ServerConfig::default()
    };
    let state = AppState::new(cfg, tdb.db.clone()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let running = serve(state, listener).await.unwrap();
    let base = running.url();
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
    let http = reqwest::Client::new();
    // The admin token has its own bucket: 5 requests pass, the 6th is refused with Retry-After.
    let mut statuses = Vec::new();
    for _ in 0..6 {
        let r = http
            .get(format!("{base}/v1/admin/workers"))
            .bearer_auth(&admin)
            .send()
            .await
            .unwrap();
        statuses.push(r.status().as_u16());
        if r.status().as_u16() == 429 {
            assert!(r.headers().get("retry-after").is_some());
        }
    }
    assert_eq!(statuses, vec![200, 200, 200, 200, 200, 429], "{statuses:?}");
    // Another actor (unauthenticated, keyed by IP) is unaffected by the admin's bucket.
    let r = http.get(format!("{base}/v1/health")).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    // Wait for one token to refill, then the metrics endpoint reports the rejection.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let r = http
        .get(format!("{base}/v1/admin/metrics"))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200, "{}", r.text().await.unwrap());
    let text = r.text().await.unwrap();
    assert!(text.contains("gg_quota_rejections_total 1"), "{text}");
    assert!(text.contains("gg_http_requests_total{method=\"GET\",route=\"/v1/admin/workers\",status_class=\"200\"} 5"), "{text}");
    assert!(
        text.contains("gg_tasks") || text.contains("gg_outbox_pending"),
        "{text}"
    );
    running.stop().await;
    tdb.drop().await;
}

async fn health_statuses(base: &str, forwarded: Option<&str>, n: usize) -> Vec<u16> {
    let http = reqwest::Client::new();
    let mut out = Vec::new();
    for _ in 0..n {
        let mut req = http.get(format!("{base}/v1/health"));
        if let Some(f) = forwarded {
            req = req.header("x-forwarded-for", f);
        }
        out.push(req.send().await.unwrap().status().as_u16());
    }
    out
}

/// Behind the reverse proxy, anonymous clients are told apart by the address the proxy forwards:
/// one client exhausting its quota does not block another. The header is believed only from a
/// trusted proxy; otherwise every request counts against the connection's address.
#[tokio::test]
async fn behind_a_trusted_proxy_each_client_has_its_own_quota() {
    for trusted in [true, false] {
        let tdb = TestDb::create_or_panic().await;
        let tmp = tempfile::tempdir().unwrap();
        let cfg = ServerConfig {
            database_url: tdb.url(),
            data_dir: tmp.path().to_path_buf(),
            network_id: "gg-proxy".into(),
            quota_requests_per_minute: 60,
            quota_burst: 3,
            trusted_proxies: if trusted {
                vec!["127.0.0.1".parse().unwrap()]
            } else {
                Vec::new()
            },
            ..ServerConfig::default()
        };
        let state = AppState::new(cfg, tdb.db.clone()).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let running = serve(state, listener).await.unwrap();
        let base = running.url();
        let a = health_statuses(&base, Some("203.0.113.7"), 4).await;
        assert_eq!(a, vec![200, 200, 200, 429], "trusted={trusted}: client A");
        let b = health_statuses(&base, Some("198.51.100.9"), 1).await;
        let expected = if trusted { 200 } else { 429 };
        assert_eq!(b, vec![expected], "trusted={trusted}: client B");
        running.stop().await;
        tdb.drop().await;
    }
}
