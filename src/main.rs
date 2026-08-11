use std::net::SocketAddr;

use clap::{Parser, Subcommand};
use memeloop_token_center::{AppState, api, config::Config, db::Database};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "memeloop-token-center")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Serve,
    Migrate,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .json()
        .init();

    let cli = Cli::parse();
    let config = Config::from_env()?;

    match cli.command {
        Command::Migrate => {
            let database = Database::connect(&config.database_url).await?;
            database.migrate().await?;
            info!("database schema is current");
        }
        Command::Serve => {
            let state = AppState::initialize(config.clone()).await?;
            let address: SocketAddr = config.listen.parse()?;
            let listener = TcpListener::bind(address).await?;
            info!(%address, "token center listening");
            axum::serve(listener, api::router(state))
                .with_graceful_shutdown(shutdown_signal())
                .await?;
        }
    }

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
