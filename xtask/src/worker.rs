//! The contributor binaries (`gg-worker`, `gg-executor`) live in their own repository,
//! [gg-worker](https://github.com/GPU-Goblin/gg-worker). The demonstration and the release build them
//! from a checkout of it: `GG_WORKER_DIR`, or `../gg-worker` next to this repository.

use std::path::PathBuf;

/// The gg-worker checkout.
pub fn dir() -> anyhow::Result<PathBuf> {
    let dir = std::env::var("GG_WORKER_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../gg-worker"));
    anyhow::ensure!(
        dir.join("Cargo.toml").exists(),
        "no gg-worker checkout at {}: clone https://github.com/GPU-Goblin/gg-worker next to this \
         repository, or set GG_WORKER_DIR",
        dir.display()
    );
    Ok(dir)
}

/// Build the worker binaries with the given profile flags (`[]` or `["--release"]`), optionally
/// with extra arguments such as `--features gg-worker/gpu`.
pub fn build(extra: &[&str]) -> anyhow::Result<()> {
    let manifest = dir()?.join("Cargo.toml");
    let manifest = manifest.to_string_lossy().into_owned();
    let mut args = vec!["build", "--manifest-path", manifest.as_str(), "--bins"];
    args.extend_from_slice(extra);
    super::run("cargo", &args)
}

/// Directory holding the worker binaries of a profile (`debug` or `release`).
pub fn target(profile: &str) -> anyhow::Result<PathBuf> {
    let base = match std::env::var("CARGO_TARGET_DIR") {
        Ok(t) if !t.is_empty() => PathBuf::from(t),
        _ => dir()?.join("target"),
    };
    Ok(base.join(profile))
}

/// Commit of the gg-worker checkout.
pub fn commit() -> Option<String> {
    let dir = dir().ok()?;
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}
