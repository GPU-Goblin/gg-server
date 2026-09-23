//! `gg-server`: control plane binary.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use gg_control::ServerConfig;

#[derive(Parser)]
#[command(name = "gg-server", about = "GPU Goblin control plane")]
struct Cli {
    /// TOML configuration file (environment `GG_*` variables override it).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the server.
    Run,
    /// Apply migrations and exit.
    Migrate,
    /// Create an admin API token (printed once) and exit.
    BootstrapAdmin {
        /// Label stored with the token.
        #[arg(long, default_value = "bootstrap")]
        label: String,
    },
    /// Print the server signing public key.
    Identity,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_target(false)
        .init();
    let cli = Cli::parse();
    let config = match ServerConfig::load(cli.config.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("configuration error: {e}");
            std::process::exit(2);
        }
    };
    let result = match cli.cmd {
        Cmd::Run => gg_control::server::run(config).await,
        Cmd::Migrate => {
            async {
                let db = gg_storage::Db::connect(&config.database_url, 2)
                    .await
                    .map_err(|e| e.to_string())?;
                db.migrate().await.map_err(|e| e.to_string())?;
                println!("migrations applied");
                Ok(())
            }
            .await
        }
        Cmd::BootstrapAdmin { label } => {
            async {
                let db = gg_storage::Db::connect(&config.database_url, 2)
                    .await
                    .map_err(|e| e.to_string())?;
                db.migrate().await.map_err(|e| e.to_string())?;
                let token = gg_control::auth::new_token(gg_control::auth::API_TOKEN_PREFIX)
                    .map_err(|e| e.to_string())?;
                let mut c = db.pool().acquire().await.map_err(|e| e.to_string())?;
                gg_storage::queries::identities::create_token(
                    &mut c,
                    &gg_control::auth::token_hash(&token),
                    "admin",
                    None,
                    &label,
                    None,
                )
                .await
                .map_err(|e| e.to_string())?;
                gg_storage::queries::misc::audit(
                    &mut c,
                    "bootstrap",
                    "admin.token.create",
                    &label,
                    serde_json::json!({}),
                )
                .await
                .map_err(|e| e.to_string())?;
                println!("{token}");
                Ok(())
            }
            .await
        }
        Cmd::Identity => {
            async {
                let key = gg_protocol::keys::SecretKey::load_or_generate(
                    &config.data_dir.join("server.key"),
                )
                .map_err(|e| e.to_string())?;
                println!("{}", key.public_key());
                Ok(())
            }
            .await
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
