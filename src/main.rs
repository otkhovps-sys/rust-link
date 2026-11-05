use std::{net::SocketAddr, path::PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use rust_link::{api, config::AppConfig, node, onedrive};

#[derive(Parser)]
#[command(
    name = "rust-link",
    version,
    about = "Lavalink v4 compatible audio node with LRU cache and OneDrive backup"
)]
struct Cli {
    /// Path to application.yml
    #[arg(short, long, env = "RUSTLINK_CONFIG", default_value = "config/application.yml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the audio node (default).
    Serve,
    /// Interactive OAuth2 device-code login; writes the OneDrive credentials file.
    OnedriveLogin {
        /// Azure app registration (client) ID with public client flows enabled.
        #[arg(long, env = "ONEDRIVE_CLIENT_ID")]
        client_id: String,
        /// `consumers` for personal OneDrive, `organizations`, `common` or a tenant GUID.
        #[arg(long, default_value = "consumers")]
        tenant: String,
        #[arg(long, env = "ONEDRIVE_CLIENT_SECRET")]
        client_secret: Option<String>,
        #[arg(long, default_value = "offline_access Files.ReadWrite")]
        scope: String,
    },
    /// Validate the configuration and exit.
    CheckConfig,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = Cli::parse();
    let config = AppConfig::load(&cli.config)?;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("{},sqlx=warn,symphonia=warn", config.logging.level)));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(config).await,
        Command::CheckConfig => {
            println!("{config:#?}");
            println!("configuration OK");
            Ok(())
        }
        Command::OnedriveLogin {
            client_id,
            tenant,
            client_secret,
            scope,
        } => {
            let path = config.onedrive.credentials_file.clone();
            onedrive::auth::device_code_login(onedrive::auth::LoginRequest {
                credentials_path: path.clone(),
                authority: config.onedrive.authority.clone(),
                client_id,
                client_secret,
                tenant,
                scope,
            })
            .await?;
            println!("OneDrive credentials saved to {}", path.display());
            Ok(())
        }
    }
}

async fn serve(config: AppConfig) -> anyhow::Result<()> {
    let address: SocketAddr = format!("{}:{}", config.server.address, config.server.port)
        .parse()
        .context("invalid server.address / server.port")?;
    let shutdown = CancellationToken::new();
    let ctx = node::build(config, shutdown.clone()).await?;

    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("cannot bind {address}"))?;
    info!(%address, "rust-link is ready (Lavalink v4 API)");

    let app = api::router(ctx.clone());
    let signal_token = shutdown.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        info!("shutdown requested");
        signal_token.cancel();
    });

    let server_token = shutdown.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { server_token.cancelled().await })
        .await?;

    for session in ctx.sessions.all() {
        session.destroy().await;
    }
    ctx.cache.index().close().await;
    info!("bye");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            warn!(%e, "cannot listen for ctrl-c");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
