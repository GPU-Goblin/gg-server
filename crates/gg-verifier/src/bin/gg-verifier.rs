//! `gg-verifier`: administered validator process.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use gg_verifier::{Verifier, VerifierConfig};

#[derive(Parser)]
#[command(name = "gg-verifier", about = "GPU Goblin validator")]
struct Cli {
    /// TOML configuration (environment `GG_*` variables override it).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the key (if absent) and print the public key.
    Init,
    /// Print the public key.
    Identity,
    /// Claim and verify until Ctrl-C.
    Run {
        /// Stop after this many verifications.
        #[arg(long)]
        max: Option<usize>,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_target(false)
        .init();
    let cli = Cli::parse();
    let cfg = match VerifierConfig::load(cli.config.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("configuration error: {e}");
            std::process::exit(2);
        }
    };
    let result: anyhow::Result<()> = match cli.cmd {
        Cmd::Init | Cmd::Identity => (|| {
            let key = gg_protocol::keys::SecretKey::load_or_generate(&cfg.key_path)?;
            println!("{}", key.public_key());
            Ok(())
        })(),
        Cmd::Run { max } => {
            async {
                let (tx, rx) = tokio::sync::watch::channel(false);
                tokio::spawn(async move {
                    gg_client::signals::terminated().await;
                    let _ = tx.send(true);
                });
                let mut v = Verifier::new(cfg).await?;
                tracing::info!(key = %v.public_key(), "validator identity");
                v.run(rx, max).await?;
                tracing::info!(decisions = v.decided.len(), "validator stopped");
                Ok(())
            }
            .await
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
