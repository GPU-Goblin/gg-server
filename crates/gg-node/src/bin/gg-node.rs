//! `gg-node`: a laboratory's lab node (ADR-0041). Holds the laboratory's datasets and checkpoints,
//! pulls the workers' outputs, steps the training runs, and answers the Python library.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use gg_node::config::ServiceConfig;

#[derive(Parser)]
#[command(name = "gg-node", about = "GPU Goblin lab node")]
struct Cli {
    /// TOML configuration (`GG_NODE_*` variables override it).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the node's key (if absent) and print its public key: the one a researcher registers.
    Init,
    /// Print the node's public key.
    Identity,
    /// Print the servers this node would work with (the configured ones, or the signed list).
    Servers,
    /// Work with every server where the key is registered, and serve the local API, until stopped.
    Run,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_target(false)
        .init();
    let cli = Cli::parse();
    let cfg = match ServiceConfig::load(cli.config.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("configuration error: {e:#}");
            std::process::exit(2);
        }
    };
    let result: anyhow::Result<()> = match cli.cmd {
        Cmd::Init | Cmd::Identity => (|| {
            if let Some(dir) = cfg.key_path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let key = gg_protocol::keys::SecretKey::load_or_generate(&cfg.key_path)?;
            println!("{}", key.public_key());
            Ok(())
        })(),
        Cmd::Servers => {
            async {
                for s in gg_node::service::servers(&cfg).await? {
                    println!("{}\t{}\t{}", s.network, s.url, s.name);
                }
                Ok(())
            }
            .await
        }
        Cmd::Run => {
            async {
                let (tx, rx) = tokio::sync::watch::channel(false);
                tokio::spawn(async move {
                    let mut term =
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                            .expect("SIGTERM handler");
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {}
                        _ = term.recv() => {}
                    }
                    tracing::info!("stopping");
                    let _ = tx.send(true);
                });
                gg_node::service::run(cfg, rx).await
            }
            .await
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
