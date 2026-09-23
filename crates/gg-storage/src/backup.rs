//! Backup and restore of the database (pg_dump/pg_restore) and of the object store directory.
//!
//! PostgreSQL client tools are located either on the host (`pg_dump` in `PATH`) or inside a docker
//! container named by `GG_PG_DOCKER_CONTAINER` (the development setup). Restoring a database drops
//! its current content; callers confirm the intent.

use std::path::Path;
use std::process::Command;

use crate::StorageError;

/// Where the PostgreSQL client tools run.
#[derive(Clone, Debug)]
pub enum PgTools {
    /// Host binaries.
    Host,
    /// `docker exec <container>`.
    Docker(String),
}

impl PgTools {
    /// Detect from the environment: `GG_PG_DOCKER_CONTAINER` wins, else host binaries.
    pub fn detect() -> Result<Self, StorageError> {
        if let Ok(c) = std::env::var("GG_PG_DOCKER_CONTAINER")
            && !c.is_empty()
        {
            return Ok(PgTools::Docker(c));
        }
        if Command::new("pg_dump")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Ok(PgTools::Host);
        }
        // The local development container (`cargo xtask db up`); another name can be given with
        // `GG_PG_CONTAINER`.
        let container =
            std::env::var("GG_PG_CONTAINER").unwrap_or_else(|_| "gpu-goblin-postgres".into());
        if Command::new("docker")
            .args(["exec", &container, "pg_dump", "--version"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Ok(PgTools::Docker(container));
        }
        Err(StorageError::NotFound("pg_dump (host or docker container)"))
    }

    fn command(&self, tool: &str) -> Command {
        match self {
            PgTools::Host => Command::new(tool),
            PgTools::Docker(c) => {
                let mut cmd = Command::new("docker");
                cmd.args(["exec", "-i", c, tool]);
                cmd
            }
        }
    }
}

/// Parse a `postgres://user:pass@host:port/db` URL into pieces usable by the tools.
struct Conn {
    user: String,
    password: String,
    host: String,
    port: String,
    db: String,
}

fn parse_url(url: &str) -> Result<Conn, StorageError> {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .ok_or(StorageError::Corrupt("database url scheme".into()))?;
    let (cred, tail) = rest
        .split_once('@')
        .ok_or(StorageError::Corrupt("database url credentials".into()))?;
    let (user, password) = cred.split_once(':').unwrap_or((cred, ""));
    let (hostport, db) = tail
        .split_once('/')
        .ok_or(StorageError::Corrupt("database url db".into()))?;
    let (host, port) = hostport.split_once(':').unwrap_or((hostport, "5432"));
    let db = db.split('?').next().unwrap_or(db);
    Ok(Conn {
        user: user.into(),
        password: password.into(),
        host: host.into(),
        port: port.into(),
        db: db.into(),
    })
}

/// Dump `url` into `out` (custom format).
pub fn dump_database(tools: &PgTools, url: &str, out: &Path) -> Result<u64, StorageError> {
    let c = parse_url(url)?;
    let (host, port) = match tools {
        PgTools::Docker(_) => ("127.0.0.1".to_owned(), "5432".to_owned()),
        PgTools::Host => (c.host.clone(), c.port.clone()),
    };
    let output = tools
        .command("pg_dump")
        .env("PGPASSWORD", &c.password)
        .args([
            "-h",
            &host,
            "-p",
            &port,
            "-U",
            &c.user,
            "-Fc",
            "--no-owner",
            "--no-privileges",
            &c.db,
        ])
        .output()?;
    if !output.status.success() {
        return Err(StorageError::Corrupt(format!(
            "pg_dump failed: {}",
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(300)
                .collect::<String>()
        )));
    }
    std::fs::write(out, &output.stdout)?;
    Ok(output.stdout.len() as u64)
}

/// Restore `dump` into the database of `url` (which must exist; its content is replaced).
pub fn restore_database(tools: &PgTools, url: &str, dump: &Path) -> Result<(), StorageError> {
    use std::io::Write;
    let c = parse_url(url)?;
    let (host, port) = match tools {
        PgTools::Docker(_) => ("127.0.0.1".to_owned(), "5432".to_owned()),
        PgTools::Host => (c.host.clone(), c.port.clone()),
    };
    let bytes = std::fs::read(dump)?;
    let mut child = tools
        .command("pg_restore")
        .env("PGPASSWORD", &c.password)
        .args([
            "-h",
            &host,
            "-p",
            &port,
            "-U",
            &c.user,
            "-d",
            &c.db,
            "--clean",
            "--if-exists",
            "--no-owner",
            "--no-privileges",
            "--exit-on-error",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&bytes)?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(StorageError::Corrupt(format!(
            "pg_restore failed: {}",
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(400)
                .collect::<String>()
        )));
    }
    Ok(())
}

/// Copy the object store directory (objects only, no temporary uploads).
pub fn copy_objects(src_root: &Path, dst_root: &Path) -> Result<u64, StorageError> {
    let src = src_root.join("objects");
    let dst = dst_root.join("objects");
    let mut n = 0;
    fn walk(from: &Path, to: &Path, n: &mut u64) -> std::io::Result<()> {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            let target = to.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                walk(&entry.path(), &target, n)?;
            } else {
                std::fs::copy(entry.path(), &target)?;
                *n += 1;
            }
        }
        Ok(())
    }
    if src.exists() {
        walk(&src, &dst, &mut n)?;
    }
    Ok(n)
}
