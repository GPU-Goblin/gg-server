//! The Windows files of a release (ADR-0045): the worker and its executor for the processor and
//! for Vulkan, the native CUDA executor (linked against the official LibTorch archive pinned by
//! `deploy/worker/libtorch-windows.sha256`, which the release carries and the installer checks; the
//! archive is not redistributed), and the application, which is also the installer. They are built for
//! `x86_64-pc-windows-msvc`, by cross-compilation in Docker (cargo-xwin, an image pinned by its
//! digest) or natively on Windows, then added to the unsigned manifest of the release, next to the
//! Linux files, before the offline signature.
//!
//! The application carries the release key the contributor trusts and the network it installs
//! for: the key is the line written in `deploy/worker/install.sh`, the one root of trust of both
//! installers; a release is refused while it is the placeholder.

use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Args;
use gg_protocol::release::{ReleaseArtifactV1, ReleaseManifestV1, hash_file};

/// cargo-xwin 0.23.1 with clang 19 (Debian), pinned: the Windows bytes depend on this image.
const XWIN_IMAGE: &str =
    "messense/cargo-xwin@sha256:9856b895265d4966f212228ba64802cf89337e2a2a537aa2533c1b8784cbc81b";
const TARGET: &str = "x86_64-pc-windows-msvc";

#[derive(Args)]
pub struct WindowsArgs {
    /// Directory of the release (as `cargo xtask release --out`): the files are written there, and
    /// added to its `release.unsigned.json` when it exists.
    #[arg(long, default_value = "var/release")]
    pub out: PathBuf,
    /// Network the release targets (the installer refuses a release for another one).
    #[arg(long, default_value = "gg-beta-1")]
    pub network: String,
    /// An `allowed_signers` file with the release key, instead of the line in
    /// `deploy/worker/install.sh` (rehearsals with a throwaway key).
    #[arg(long)]
    pub allowed_signers: Option<PathBuf>,
    /// Build with the local toolchain (on Windows) instead of cross-compiling in Docker.
    #[arg(long)]
    pub native: bool,
    /// An unpacked copy of the pinned Windows LibTorch archive (its `libtorch` directory), instead
    /// of downloading and checking it here.
    #[arg(long)]
    pub libtorch: Option<PathBuf>,
}

/// The files, as the installer (`gg-desk/src/install.rs`) and the site name them.
const FILES: [(&str, &str, &str); 7] = [
    // (build, file in the build directory, name in the release)
    ("cpu", "gg-worker.exe", "gg-worker-windows.exe"),
    ("cpu", "gg-executor.exe", "gg-executor-windows.exe"),
    ("gpu", "gg-worker.exe", "gg-worker-gpu-windows.exe"),
    ("gpu", "gg-executor.exe", "gg-executor-gpu-windows.exe"),
    // Paired by the installer with the GPU build of the worker (as install.sh --cuda on Linux).
    ("cuda", "gg-executor.exe", "gg-executor-cuda-windows.exe"),
    ("cpu", "gg-desk.exe", "gg-desk-windows.exe"),
    // What the site offers to download: the same program, under the name people look for.
    ("cpu", "gg-desk.exe", "GPUGoblinSetup.exe"),
];

/// The release key: the `allowed_signers` line the installers trust.
fn release_key(args: &WindowsArgs, workspace: &Path) -> anyhow::Result<String> {
    let line = match &args.allowed_signers {
        Some(file) => std::fs::read_to_string(file)?
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .unwrap_or_default()
            .to_owned(),
        None => {
            let script = std::fs::read_to_string(workspace.join("deploy/worker/install.sh"))?;
            script
                .lines()
                .find_map(|l| l.strip_prefix("RELEASE_ALLOWED_SIGNERS='"))
                .and_then(|l| l.strip_suffix('\''))
                .unwrap_or_default()
                .to_owned()
        }
    };
    anyhow::ensure!(
        !line.contains("REPLACE_WITH"),
        "deploy/worker/install.sh carries no release key yet (RELEASE_ALLOWED_SIGNERS): write it \
         there first (docs/RELEASE.md), or pass --allowed-signers for a rehearsal"
    );
    anyhow::ensure!(
        line.starts_with("release@gpu-goblin namespaces=\"gg-release\" ssh-ed25519 "),
        "unexpected release key line: {line}"
    );
    Ok(line)
}

/// The four builds, as one shell script (run in the container, or here with `--native`);
/// `LIBTORCH` names the unpacked archive for the CUDA one.
fn script(target_dir: &str) -> String {
    format!(
        "set -e\n\
         cargo build --locked --release --target {TARGET} --target-dir {target_dir}/cpu -p gg-worker --bins\n\
         cargo build --locked --release --target {TARGET} --target-dir {target_dir}/gpu -p gg-worker --bins --features gg-worker/gpu\n\
         cargo build --locked --release --target {TARGET} --target-dir {target_dir}/cuda -p gg-worker --bin gg-executor --features gg-worker/cuda\n\
         cargo build --locked --release --target {TARGET} --target-dir {target_dir}/cpu -p gg-desk\n"
    )
}

/// torch-sys hands the C++ compiler an MSBuild property (`/p:DefineConstants=X`) that cl.exe
/// ignores and clang-cl takes for a file name: in the container, a clang-cl placed first on the
/// path passes it as the define it means, then runs the real one.
const CLANG_CL_WRAPPER: &str = r#"mkdir -p /tmp/wrap
cat > /tmp/wrap/clang-cl <<'W'
#!/bin/sh
for a in "$@"; do shift; case "$a" in /p:DefineConstants=*) set -- "$@" "-D${a#/p:DefineConstants=}";; *) set -- "$@" "$a";; esac; done
PATH=$(printf %s "$PATH" | sed "s#/tmp/wrap:##g"); export PATH
exec clang-cl "$@"
W
chmod +x /tmp/wrap/clang-cl
export PATH=/tmp/wrap:$PATH
"#;

/// The pinned Windows LibTorch: url, size, digest (the lines of the signed pin file).
fn libtorch_pin(workspace: &Path) -> anyhow::Result<(String, u64, String, String)> {
    let pin = std::fs::read_to_string(workspace.join("deploy/worker/libtorch-windows.sha256"))?;
    let comment = |k: &str| {
        pin.lines()
            .find_map(|l| l.strip_prefix(&format!("# {k}: ")))
            .map(str::to_owned)
    };
    let url = comment("url").ok_or_else(|| anyhow::anyhow!("no url in the LibTorch pin"))?;
    let size: u64 = comment("size")
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("no size in the LibTorch pin"))?;
    let (digest, name) = pin
        .lines()
        .find(|l| !l.starts_with('#') && !l.trim().is_empty())
        .and_then(|l| l.split_once("  "))
        .ok_or_else(|| anyhow::anyhow!("no digest in the LibTorch pin"))?;
    Ok((url, size, digest.to_owned(), name.to_owned()))
}

/// The unpacked pinned LibTorch, downloaded and checked once (kept by digest under `target/`).
fn libtorch(workspace: &Path, target_dir: &Path) -> anyhow::Result<PathBuf> {
    let (url, size, digest, name) = libtorch_pin(workspace)?;
    let dir = target_dir.join("libtorch").join(&digest);
    if dir.join(".verified").is_file() {
        return Ok(dir.join("libtorch"));
    }
    std::fs::create_dir_all(&dir)?;
    let archive = dir.join(&name);
    println!("-- LibTorch for Windows ({} MB): {url}", size / 1_000_000);
    super::run(
        "curl",
        &[
            "-fSL",
            "--proto",
            "=https",
            "--retry",
            "3",
            "-o",
            &archive.to_string_lossy(),
            &url,
        ],
    )?;
    let (got_size, got) = hash_file(&archive).map_err(|e| anyhow::anyhow!("{e}"))?;
    anyhow::ensure!(
        got_size == size && got.to_string() == digest,
        "the LibTorch archive does not match the pin ({got_size} bytes, sha256 {got})"
    );
    super::run(
        "unzip",
        &[
            "-q",
            "-o",
            &archive.to_string_lossy(),
            "-d",
            &dir.to_string_lossy(),
        ],
    )?;
    std::fs::remove_file(&archive)?;
    std::fs::write(dir.join(".verified"), &digest)?;
    Ok(dir.join("libtorch"))
}

pub fn run(args: WindowsArgs) -> anyhow::Result<()> {
    // The worker's workspace: the gg-worker checkout (GG_WORKER_DIR, or ../gg-worker).
    let workspace = super::worker::dir()?.canonicalize()?;
    let key = release_key(&args, &workspace)?;
    let target_dir = workspace.join("target/windows");
    std::fs::create_dir_all(&target_dir)?;
    let libtorch = match &args.libtorch {
        Some(d) => d.canonicalize()?,
        None => libtorch(&workspace, &target_dir)?,
    };
    anyhow::ensure!(
        libtorch.join("lib/torch_cuda.lib").is_file(),
        "{} holds no lib/torch_cuda.lib: not the Windows CUDA LibTorch",
        libtorch.display()
    );
    println!("-- Windows build for {TARGET}, network {}", args.network);
    let status = if args.native {
        Command::new("sh")
            .arg("-c")
            .arg(script(&target_dir.to_string_lossy()))
            .current_dir(&workspace)
            .env("GG_RELEASE_ALLOWED_SIGNERS", &key)
            .env("GG_RELEASE_NETWORK", &args.network)
            .env("LIBTORCH", &libtorch)
            .status()?
    } else {
        // Sources read-only; the target directory written as this user's (chown at the end);
        // the toolchain, the Windows SDK and the crates kept in a volume between runs.
        let uid = String::from_utf8(Command::new("id").arg("-u").output()?.stdout)?;
        let gid = String::from_utf8(Command::new("id").arg("-g").output()?.stdout)?;
        let body = format!(
            "export PATH=/cache/cargo/bin:$PATH\n\
             [ -x /cache/cargo/bin/cargo ] || {{ cp -r /usr/local/cargo/. /cache/cargo/; cp -r /usr/local/rustup/. /cache/rustup/; }}\n\
             trap 'chown -R {}:{} /target' EXIT\n{CLANG_CL_WRAPPER}{}",
            uid.trim(),
            gid.trim(),
            script("/target").replace("cargo build", "cargo xwin build")
        );
        Command::new("docker")
            .args(["run", "--rm"])
            .arg("-v")
            .arg(format!("{}:/src:ro", workspace.display()))
            .arg("-v")
            .arg(format!("{}:/target", target_dir.display()))
            .args(["-v", "ggxwin-cache:/cache"])
            .arg("-v")
            .arg(format!("{}:/libtorch:ro", libtorch.display()))
            .args(["-e", "LIBTORCH=/libtorch"])
            .args([
                "-e",
                "CARGO_HOME=/cache/cargo",
                "-e",
                "RUSTUP_HOME=/cache/rustup",
            ])
            .args(["-e", "XWIN_CACHE_DIR=/cache/xwin"])
            .args([
                "-e",
                "GG_RELEASE_ALLOWED_SIGNERS",
                "-e",
                "GG_RELEASE_NETWORK",
            ])
            .args(["-e", "CARGO_PROFILE_RELEASE_STRIP"])
            .env("GG_RELEASE_ALLOWED_SIGNERS", &key)
            .env("GG_RELEASE_NETWORK", &args.network)
            .args(["-w", "/src", XWIN_IMAGE, "sh", "-c", &body])
            .status()?
    };
    anyhow::ensure!(status.success(), "the Windows build failed ({status})");

    std::fs::create_dir_all(&args.out)?;
    std::fs::copy(
        workspace.join("deploy/worker/libtorch-windows.sha256"),
        args.out.join("libtorch-windows.sha256"),
    )?;
    for (build, file, name) in FILES {
        let from = target_dir
            .join(build)
            .join(TARGET)
            .join("release")
            .join(file);
        std::fs::copy(&from, args.out.join(name))
            .map_err(|e| anyhow::anyhow!("{}: {e}", from.display()))?;
    }
    let manifest_path = args.out.join("release.unsigned.json");
    if !manifest_path.exists() {
        println!(
            "Windows files written to {}; no release.unsigned.json there: add them with\n  gg release add <manifest> {}",
            args.out.display(),
            FILES.map(|f| f.2).join(" ") + " libtorch-windows.sha256"
        );
        return Ok(());
    }
    let mut manifest: ReleaseManifestV1 =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path)?)?;
    anyhow::ensure!(
        manifest.network.as_str() == args.network,
        "the manifest is for network {}, the Windows build for {}",
        manifest.network.as_str(),
        args.network
    );
    for name in FILES
        .iter()
        .map(|f| f.2)
        .chain(std::iter::once("libtorch-windows.sha256"))
    {
        anyhow::ensure!(
            !manifest.artifacts.iter().any(|a| a.name == name),
            "{name} is already in the manifest"
        );
        let (size, sha256) = hash_file(&args.out.join(name)).map_err(|e| anyhow::anyhow!("{e}"))?;
        println!("  {name}: {size} bytes, sha256 {sha256}");
        manifest.artifacts.push(ReleaseArtifactV1 {
            name: name.to_owned(),
            size,
            sha256,
        });
    }
    manifest.artifacts.sort_by(|a, b| a.name.cmp(&b.name));
    manifest
        .validate_structure()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest)? + "\n",
    )?;
    println!(
        "{}: {} files, still unsigned",
        manifest_path.display(),
        manifest.artifacts.len()
    );
    Ok(())
}
