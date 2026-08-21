use std::{net::SocketAddr, process::ExitCode, time::Duration};

use clap::{Parser, Subcommand};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::Database,
    worker,
};
use tokio::{net::TcpListener, sync::watch};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[global_allocator]
#[cfg(not(target_env = "msvc"))]
static GLOBAL_ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(all(not(target_env = "msvc"), not(target_env = "musl")))]
union JemallocConfigPointer {
    byte: &'static u8,
    character: &'static std::ffi::c_char,
}

// This gateway favors a predictable container footprint over maximum allocator
// throughput. A small arena count limits fragmentation, while the background
// purger and short decay release transient stream/image pages promptly. Disable
// per-thread caches as well: the gateway's low-rate, long-lived Tokio workload
// otherwise strands small allocations across worker-thread caches after the
// large streaming probes have fragmented the arenas.
#[cfg(all(not(target_env = "msvc"), not(target_env = "musl")))]
#[unsafe(export_name = "_rjem_malloc_conf")]
static JEMALLOC_CONFIG: Option<&'static std::ffi::c_char> = Some(unsafe {
    JemallocConfigPointer {
        byte: &b"abort_conf:true,background_thread:true,narenas:2,tcache:false,dirty_decay_ms:1000,muzzy_decay_ms:0\0"[0],
    }
    .character
});

#[derive(Debug, Parser)]
#[command(name = "memeloop-token-center")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Serve {
        #[arg(long, value_enum, default_value_t)]
        role: RuntimeRole,
    },
    Migrate,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .json()
        .init();

    info!(
        version = memeloop_token_center::metrics::BUILD_VERSION,
        revision = memeloop_token_center::metrics::BUILD_GIT_SHA,
        build_timestamp = memeloop_token_center::metrics::BUILD_TIMESTAMP,
        target = memeloop_token_center::metrics::BUILD_TARGET,
        "token center build"
    );

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error_code) => {
            error!(
                error_code,
                "token center stopped before completing its lifecycle"
            );
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), &'static str> {
    let cli = Cli::parse();
    let config = Config::from_env().map_err(|_| "configuration_invalid")?;

    match cli.command {
        Command::Migrate => {
            let database = Database::connect_for_migration(
                &config.database_url,
                config.database_max_connections,
            )
            .await
            .map_err(|_| "database_connect_failed")?;
            database
                .migrate()
                .await
                .map_err(|_| "database_migration_failed")?;
            info!("database schema is current");
        }
        Command::Serve { role } => {
            let state = AppState::initialize(config.clone())
                .await
                .map_err(|_| "application_initialization_failed")?;
            let address: SocketAddr = config
                .listen
                .parse()
                .map_err(|_| "listen_address_invalid")?;
            let listener = TcpListener::bind(address)
                .await
                .map_err(|_| "listen_bind_failed")?;
            let (worker_shutdown, mut worker_task) = if role.runs_worker() {
                let (sender, receiver) = watch::channel(false);
                let task = tokio::spawn(worker::run_until_shutdown(state.clone(), receiver));
                (Some(sender), Some(task))
            } else {
                (None, None)
            };
            info!(%address, ?role, "token center listening");
            let result = memeloop_token_center::server::serve(
                listener,
                api::router_for_role(state, role),
                shutdown_signal(),
            )
            .await;
            if let Some(sender) = worker_shutdown {
                let _ = sender.send(true);
            }
            if let Some(task) = worker_task.as_mut() {
                match tokio::time::timeout(Duration::from_secs(30), &mut *task).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => error!(
                        error_code = "worker_task_failed",
                        "background worker stopped unexpectedly"
                    ),
                    Err(_) => {
                        warn!(
                            error_code = "worker_shutdown_timeout",
                            "background worker did not stop before the shutdown deadline"
                        );
                        task.abort();
                        let _ = task.await;
                    }
                }
            }
            result.map_err(|_| "http_server_failed")?;
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
