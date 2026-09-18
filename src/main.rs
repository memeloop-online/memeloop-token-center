use std::{net::SocketAddr, process::ExitCode};

use clap::{Parser, Subcommand};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{
        Database, FAILED_REQUEST_COST_BACKFILL_MAX_BATCH_SIZE, FailedRequestCostBackfillCursor,
        FailedRequestCostBackfillInput,
    },
    worker::{self, wait_for_server_shutdown},
};
use tokio::{net::TcpListener, sync::watch};
use tracing::{error, info};
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
        byte: &b"abort_conf:true,background_thread:true,narenas:2,tcache:false,dirty_decay_ms:1000,muzzy_decay_ms:0,prof:true,prof_active:false,lg_prof_sample:19\0"[0],
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
    /// Audit or correct one bounded batch of historical failed-request cost projections.
    /// Without --apply this is a read-only dry run; resume from the returned JSON cursor.
    BackfillFailedRequestCosts {
        /// Persist fact corrections and rebuild affected aggregate dimensions.
        #[arg(long)]
        apply: bool,
        /// Maximum eligible facts inspected in this transaction.
        #[arg(
            long,
            default_value_t = 100,
            value_parser = clap::value_parser!(i64).range(1..=FAILED_REQUEST_COST_BACKFILL_MAX_BATCH_SIZE)
        )]
        batch_size: i64,
        /// Resume strictly after this fact timestamp; must accompany --after-request-id.
        #[arg(long, requires = "after_request_id")]
        after_created_at: Option<i64>,
        /// Resume strictly after this request id at --after-created-at.
        #[arg(long, requires = "after_created_at")]
        after_request_id: Option<String>,
    },
    /// Initialize an empty shared inventory without replacing existing data.
    #[cfg(feature = "experimental-plugin-revisions")]
    PreparePluginInventory {
        #[arg(long)]
        directory: std::path::PathBuf,
        /// Reader roles validate the file without creating it.
        #[arg(long)]
        read_only: bool,
    },
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
    #[cfg(feature = "experimental-plugin-revisions")]
    if let Command::PreparePluginInventory {
        directory,
        read_only,
    } = &cli.command
    {
        return if *read_only {
            memeloop_token_center::plugin::application::prepare::check_inventory_directory(
                directory,
            )
        } else {
            memeloop_token_center::plugin::application::prepare::prepare_inventory_directory(
                directory,
            )
        }
        .map_err(|_| "plugin_inventory_preparation_failed");
    }
    let config = Config::from_env().map_err(|_| "configuration_invalid")?;

    match cli.command {
        #[cfg(feature = "experimental-plugin-revisions")]
        Command::PreparePluginInventory { .. } => {
            unreachable!("handled before configuration loading")
        }
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
        Command::BackfillFailedRequestCosts {
            apply,
            batch_size,
            after_created_at,
            after_request_id,
        } => {
            let database = Database::connect_with_max(&config.database_url, 1)
                .await
                .map_err(|_| "database_connect_failed")?;
            let after = after_created_at
                .zip(after_request_id)
                .map(|(created_at, request_id)| FailedRequestCostBackfillCursor {
                    created_at,
                    request_id,
                });
            let report = database
                .backfill_failed_request_costs(FailedRequestCostBackfillInput {
                    apply,
                    batch_size,
                    after,
                })
                .await
                .map_err(|_| "failed_request_cost_backfill_failed")?;
            let output = serde_json::to_string(&report)
                .map_err(|_| "failed_request_cost_backfill_report_failed")?;
            println!("{output}");
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
            let mut worker_failed = false;
            let result = memeloop_token_center::server::serve(
                listener,
                api::router_for_role(state, role),
                async {
                    worker_failed =
                        wait_for_server_shutdown(shutdown_signal(), &mut worker_task).await;
                },
            )
            .await;
            if let Some(sender) = worker_shutdown {
                let _ = sender.send(true);
            }
            // The worker owns its deadline and joins all roles. A second
            // timeout here could interrupt its abort-and-join cleanup.
            if let Some(task) = worker_task {
                worker_failed |= task.await.is_err();
            }
            if worker_failed {
                return Err("worker_task_failed");
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

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn failed_request_cost_backfill_is_explicit_bounded_and_dry_run_by_default() {
        let parsed =
            Cli::try_parse_from(["memeloop-token-center", "backfill-failed-request-costs"])
                .unwrap();
        let Command::BackfillFailedRequestCosts {
            apply,
            batch_size,
            after_created_at,
            after_request_id,
        } = parsed.command
        else {
            panic!("expected failed request cost backfill command");
        };
        assert!(!apply);
        assert_eq!(batch_size, 100);
        assert_eq!(after_created_at, None);
        assert_eq!(after_request_id, None);

        assert!(
            Cli::try_parse_from([
                "memeloop-token-center",
                "backfill-failed-request-costs",
                "--after-created-at",
                "1",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "memeloop-token-center",
                "backfill-failed-request-costs",
                "--batch-size",
                "1001",
            ])
            .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn worker_failure_stops_server_without_an_external_signal() {
        for panic in [false, true] {
            let mut task = Some(tokio::spawn(async move {
                assert!(!panic, "injected worker failure");
            }));
            assert!(wait_for_server_shutdown(std::future::pending(), &mut task).await);
            assert!(task.is_none());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn external_shutdown_preserves_worker_owned_cleanup_deadline() {
        let (stop, mut shutdown) = watch::channel(false);
        let (joined, observed) = tokio::sync::oneshot::channel();
        let mut task = Some(tokio::spawn(async move {
            shutdown.changed().await.unwrap();
            // Deliberately exceeds the removed outer timeout. Main must
            // preserve the worker's deadline and its final join boundary.
            tokio::time::sleep(std::time::Duration::from_secs(31)).await;
            joined.send(()).unwrap();
        }));
        assert!(!wait_for_server_shutdown(async {}, &mut task).await);
        stop.send(true).unwrap();
        task.unwrap().await.unwrap();
        observed.await.unwrap();
    }
}
