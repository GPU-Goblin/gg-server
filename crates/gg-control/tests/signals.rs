//! systemd stops a service with SIGTERM: the server must stop cleanly on it, not only on Ctrl-C.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use gg_storage::testing::TestDb;

#[tokio::test]
async fn the_server_stops_cleanly_on_sigterm() {
    let tdb = TestDb::create_or_panic().await;
    let tmp = tempfile::tempdir().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let bin = env!("CARGO_BIN_EXE_gg-server");
    let env = |c: &mut Command| {
        c.env("GG_DATABASE_URL", tdb.url())
            .env("GG_DATA_DIR", tmp.path())
            .env("GG_BIND_ADDR", format!("127.0.0.1:{port}"))
            .env("GG_NETWORK_ID", "gg-signals")
            .env("RUST_LOG", "info");
    };
    let mut migrate = Command::new(bin);
    env(&mut migrate);
    assert!(migrate.arg("migrate").status().unwrap().success());
    let mut run = Command::new(bin);
    env(&mut run);
    let mut child = run
        .arg("run")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let health = format!("http://127.0.0.1:{port}/v1/health");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if reqwest::get(&health)
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            break;
        }
        assert!(Instant::now() < deadline, "server did not come up");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let term = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(term.success());
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "server still running 20 s after SIGTERM"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let out = child.wait_with_output().unwrap();
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(status.success(), "exit {status}: {logs}");
    assert!(logs.contains("shutdown requested"), "{logs}");
    tdb.drop().await;
}
