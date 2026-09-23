//! Development PostgreSQL in docker (no host privileges, bound to 127.0.0.1:5434).

use std::process::Command;

/// Default name of the development container; `GG_PG_CONTAINER` overrides it.
pub const DEFAULT_CONTAINER: &str = "gpu-goblin-postgres";

/// Name of the development container.
pub fn container() -> String {
    std::env::var("GG_PG_CONTAINER").unwrap_or_else(|_| DEFAULT_CONTAINER.into())
}
pub const PORT: u16 = 5434;
pub const URL: &str = "postgres://gg:gg_dev_password@127.0.0.1:5434/gpu_goblin";

fn docker(args: &[&str]) -> anyhow::Result<std::process::Output> {
    Ok(Command::new("docker").args(args).output()?)
}

fn exists() -> anyhow::Result<bool> {
    let out = docker(&["ps", "-a", "--format", "{{.Names}}"])?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|l| l == container()))
}

fn running() -> anyhow::Result<bool> {
    let out = docker(&["ps", "--format", "{{.Names}}"])?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|l| l == container()))
}

/// Ensure `.env` exists (copied from `.env.example`) so tests find `DATABASE_URL`.
pub fn ensure_env() -> anyhow::Result<()> {
    if !std::path::Path::new(".env").exists() {
        std::fs::copy(".env.example", ".env")?;
        println!("created .env from .env.example");
    }
    Ok(())
}

pub fn up() -> anyhow::Result<()> {
    ensure_env()?;
    if running()? {
        println!("{} already running on 127.0.0.1:{PORT}", container());
        return wait_ready();
    }
    if exists()? {
        let out = docker(&["start", &container()])?;
        anyhow::ensure!(
            out.status.success(),
            "docker start failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    } else {
        let port = format!("127.0.0.1:{PORT}:5432");
        let name = container();
        let out = docker(&[
            "run",
            "-d",
            "--name",
            &name,
            "-e",
            "POSTGRES_USER=gg",
            "-e",
            "POSTGRES_PASSWORD=gg_dev_password",
            "-e",
            "POSTGRES_DB=gpu_goblin",
            "-p",
            &port,
            // Docker's default 64 MB of shared memory ran out under the full test suite (hundreds
            // of test databases, parallel queries), and a backend crashed.
            "--shm-size",
            "256m",
            "postgres:17-alpine",
        ])?;
        anyhow::ensure!(
            out.status.success(),
            "docker run failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    wait_ready()?;
    println!("{} ready: {URL}", container());
    Ok(())
}

fn wait_ready() -> anyhow::Result<()> {
    for _ in 0..60 {
        let out = docker(&["exec", &container(), "pg_isready", "-U", "gg"])?;
        if out.status.success() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    anyhow::bail!("PostgreSQL did not become ready in 30s")
}

pub fn down() -> anyhow::Result<()> {
    if running()? {
        docker(&["stop", &container()])?;
    }
    println!("{} stopped (data kept)", container());
    Ok(())
}

pub fn reset() -> anyhow::Result<()> {
    if exists()? {
        docker(&["rm", "-f", &container()])?;
    }
    up()
}

pub fn status() -> anyhow::Result<()> {
    println!(
        "container: {} | running: {} | url: {URL}",
        if exists()? { "present" } else { "absent" },
        running()?
    );
    Ok(())
}

/// The development database URL split into (user, password, host:port, database).
pub fn url_parts() -> (String, String, String, String) {
    let url = database_url();
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, tail) = rest.split_once('@').unwrap_or(("gg:gg_dev_password", rest));
    let (user, password) = auth.split_once(':').unwrap_or((auth, ""));
    let (host, db) = tail.split_once('/').unwrap_or((tail, "gpu_goblin"));
    (user.into(), password.into(), host.into(), db.into())
}

pub fn database_url() -> String {
    std::env::var("DATABASE_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| URL.to_owned())
}

pub fn backup(out: &str, data_dir: &str) -> anyhow::Result<()> {
    let out = std::path::Path::new(out);
    std::fs::create_dir_all(out)?;
    let tools = gg_storage::backup::PgTools::detect().map_err(|e| anyhow::anyhow!("{e}"))?;
    let bytes = gg_storage::backup::dump_database(&tools, &database_url(), &out.join("db.dump"))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let n = gg_storage::backup::copy_objects(
        &std::path::Path::new(data_dir).join("objects"),
        &out.join("objects"),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let key = std::path::Path::new(data_dir).join("server.key");
    if key.exists() {
        std::fs::copy(&key, out.join("server.key"))?;
        println!("server.key copied (protect this file: it signs receipts)");
    }
    println!(
        "backup written to {}: database {bytes} bytes, {n} objects",
        out.display()
    );
    Ok(())
}

pub fn restore(from: &str, data_dir: &str, yes: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        yes,
        "restoring replaces the current database content; pass --yes to confirm"
    );
    let from = std::path::Path::new(from);
    let tools = gg_storage::backup::PgTools::detect().map_err(|e| anyhow::anyhow!("{e}"))?;
    gg_storage::backup::restore_database(&tools, &database_url(), &from.join("db.dump"))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let n = gg_storage::backup::copy_objects(
        &from.join("objects"),
        &std::path::Path::new(data_dir).join("objects"),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("restored database and {n} objects into {data_dir}");
    Ok(())
}
