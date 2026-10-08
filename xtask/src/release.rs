//! `cargo xtask release`: build the distributed executables and sign a manifest of exactly what was
//! built (P8).
//!
//! Updates stay manual and verifiable. This command produces bytes and a signature; it installs
//! nothing, contacts nothing, and no other component acts on its output automatically. A contributor
//! checks the release with `gg release verify` against a key they chose to trust.

use std::path::{Path, PathBuf};

use clap::Args;
use gg_core::{NetworkId, UnixMillis};
use gg_protocol::domain::Domain;
use gg_protocol::keys::SecretKey;
use gg_protocol::release::{ReleaseArtifactV1, ReleaseManifestV1, hash_file};
use gg_protocol::signed::Signed;

/// Executables a contributor or an operator actually runs.
const BINARIES: [&str; 7] = [
    "gg",
    "gg-server",
    "gg-worker",
    "gg-executor",
    "gg-verifier",
    "gg-node",
    "gg-desk",
];
/// Those built from the gg-worker repository.
const WORKER_BINARIES: [&str; 3] = ["gg-worker", "gg-executor", "gg-desk"];

#[derive(Args)]
pub struct ReleaseArgs {
    /// Build and write an **unsigned** manifest, to be signed on an offline machine with
    /// `gg release sign` (docs/DECISIONS.md, D3). This is the default: the signing key must not live
    /// on the build machine.
    #[arg(long, default_value_t = true)]
    pub unsigned: bool,
    /// Sign here with this key instead. Only for a private deployment: a key on the build machine
    /// signs whatever a compromise of that machine produces.
    #[arg(long, conflicts_with = "unsigned")]
    pub key: Option<PathBuf>,
    /// Network this build targets.
    #[arg(long, default_value = "gg-dev-local")]
    pub network: String,
    /// Output directory for the binaries and the manifest.
    #[arg(long, default_value = "var/release")]
    pub out: PathBuf,
    /// Free-text notes recorded in the manifest (no secrets).
    #[arg(long, default_value = "")]
    pub notes: String,
    /// Reuse the binaries already in `target/release` instead of building.
    #[arg(long)]
    pub no_build: bool,
}

pub fn run(args: ReleaseArgs) -> anyhow::Result<()> {
    let network = NetworkId::new(&args.network).map_err(|e| anyhow::anyhow!("{e}"))?;
    if !args.no_build {
        println!("-- building the distributed executables (release profile)");
        super::run("cargo", &["build", "--workspace", "--bins", "--release"])?;
        super::worker::build(&["--release"])?;
    }
    std::fs::create_dir_all(&args.out)?;
    let built = Path::new("target/release");
    let worker_built = super::worker::target("release")?;
    let mut artifacts = Vec::new();
    for name in BINARIES {
        let from = if WORKER_BINARIES.contains(&name) {
            worker_built.join(name)
        } else {
            built.join(name)
        };
        anyhow::ensure!(
            from.exists(),
            "{} is missing: build it before publishing",
            from.display()
        );
        let to = args.out.join(name);
        std::fs::copy(&from, &to)?;
        let (size, sha256) = hash_file(&to).map_err(|e| anyhow::anyhow!("{e}"))?;
        println!("  {name}: {size} bytes, sha256 {sha256}");
        artifacts.push(ReleaseArtifactV1 {
            name: name.to_owned(),
            size,
            sha256,
        });
    }
    artifacts.sort_by(|a, b| a.name.cmp(&b.name));

    let manifest = ReleaseManifestV1 {
        protocol_version: gg_protocol::PROTOCOL_VERSION,
        network: network.clone(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        git_commit: git_commit(),
        toolchain: toolchain(),
        built_at: UnixMillis::now(),
        artifacts,
        notes: args.notes.clone(),
    };
    manifest
        .validate_structure()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    match &args.key {
        // Private deployments only: the key sits next to the build.
        Some(key_path) => {
            let key = SecretKey::load_or_generate(key_path).map_err(|e| anyhow::anyhow!("{e}"))?;
            let signed = Signed::sign(&key, &network, Domain::Release, manifest)?;
            let path = args.out.join("release.json");
            std::fs::write(&path, serde_json::to_string_pretty(&signed)? + "\n")?;
            println!(
                "\nmanifest signed here: {}\npublic key (publish it out of band): {}\n\nverify:\n  gg release verify {} --key {} --network {}",
                path.display(),
                key.public_key(),
                path.display(),
                key.public_key(),
                network
            );
        }
        // Default: nothing signs here. The manifest travels to the offline machine.
        None => {
            let path = args.out.join("release.unsigned.json");
            std::fs::write(&path, serde_json::to_string_pretty(&manifest)? + "\n")?;
            println!(
                "\nunsigned manifest: {}\n\non the offline machine (it needs only the gg binary and the key; create the key there once with\n`gg release keygen --out <removable medium>/release.key`):\n  gg release sign {} --key <removable medium>/release.key --out release.json\n\nthen bring release.json back next to the binaries and verify:\n  gg release verify {}/release.json --key <public key> --network {}",
                path.display(),
                path.display(),
                args.out.display(),
                network
            );
        }
    }
    Ok(())
}

/// Commits of both repositories the binaries come from, abbreviated to fit the manifest field
/// (64 bytes): `gg-server:<12 hex> gg-worker:<12 hex>`.
fn git_commit() -> String {
    let server = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
    let short = |c: Option<String>| {
        c.map(|c| c.chars().take(12).collect::<String>())
            .unwrap_or_else(|| "unknown".into())
    };
    format!(
        "gg-server:{} gg-worker:{}",
        short(server),
        short(super::worker::commit())
    )
}

fn toolchain() -> String {
    std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".into())
        .chars()
        .take(64)
        .collect()
}
