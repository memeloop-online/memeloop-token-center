use std::{net::SocketAddr, process::ExitCode};

use clap::{Parser, Subcommand};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{
        Database, FAILED_REQUEST_COST_BACKFILL_MAX_BATCH_SIZE, FailedRequestCostBackfillCursor,
        FailedRequestCostBackfillExactManifest, FailedRequestCostBackfillInput,
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
    #[command(
        about = "Preview an exact USD contract-ceiling failure manifest; --apply only changes statistical projections"
    )]
    BackfillExactFailedRequestCosts {
        /// Each row binds expected_status_code (499/502/504) and expected_error_code together.
        /// Omit both for legacy 499/client_cancelled; null or empty error means no error code.
        #[arg(long)]
        manifest: std::path::PathBuf,
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        cost_only: bool,
    },
    /// Audit or correct one bounded batch of historical failed-request cost projections.
    /// Without --apply this is a read-only dry run; resume from the returned JSON cursor.
    BackfillFailedRequestCosts {
        /// Persist fact corrections and rebuild affected aggregate dimensions.
        #[arg(long)]
        apply: bool,
        /// Inclusive request creation timestamp in milliseconds.
        #[arg(long)]
        from_created_at: i64,
        /// Exclusive request creation timestamp in milliseconds.
        #[arg(long)]
        to_created_at: i64,
        /// Audited request id whose legacy NULL usage basis is known to be a reservation ceiling.
        #[arg(long = "confirmed-legacy-null-request-id")]
        confirmed_legacy_null_request_ids: Vec<String>,
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
    /// Internal release-package check for the installer, runtime library path,
    /// and Cosign companion. It performs no registry or database access.
    #[cfg(all(
        feature = "experimental-plugin-revisions",
        feature = "plugin-distribution"
    ))]
    #[command(hide = true)]
    VerifyPluginRuntime,
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
    #[cfg(all(
        feature = "experimental-plugin-revisions",
        feature = "plugin-distribution"
    ))]
    if matches!(&cli.command, Command::VerifyPluginRuntime) {
        return memeloop_token_center::plugin_distribution::verify_plugin_runtime()
            .await
            .map_err(|_| "plugin_runtime_verification_failed");
    }
    let config = Config::from_env().map_err(|_| "configuration_invalid")?;

    match cli.command {
        #[cfg(feature = "experimental-plugin-revisions")]
        Command::PreparePluginInventory { .. } => {
            unreachable!("handled before configuration loading")
        }
        #[cfg(all(
            feature = "experimental-plugin-revisions",
            feature = "plugin-distribution"
        ))]
        Command::VerifyPluginRuntime => {
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
            if let Err(error) = database
                .convert_sealed_state(config.key_pepper.as_bytes())
                .await
            {
                tracing::error!(%error, "converting sealed gateway state failed");
            }
            info!("database schema is current");
        }
        Command::BackfillExactFailedRequestCosts {
            manifest,
            apply,
            cost_only,
        } => {
            use tokio::io::AsyncReadExt;

            let file = tokio::fs::File::open(manifest)
                .await
                .map_err(|_| "exact_projection_manifest_unreadable")?;
            let mut bytes = Vec::new();
            file.take(524_289)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| "exact_projection_manifest_unreadable")?;
            if bytes.len() > 524_288 {
                return Err("exact_projection_manifest_too_large");
            }
            let manifest: FailedRequestCostBackfillExactManifest =
                serde_json::from_slice(&bytes).map_err(|_| "exact_projection_manifest_invalid")?;
            let database = Database::connect_with_max(&config.database_url, 1)
                .await
                .map_err(|_| "database_connect_failed")?;
            let operation = if cost_only {
                database
                    .backfill_failed_request_costs_exact_cost_only(manifest, apply)
                    .await
            } else {
                database
                    .backfill_failed_request_costs_exact(manifest, apply)
                    .await
            };
            let report = operation.map_err(|_| "exact_projection_backfill_failed")?;
            println!(
                "{}",
                serde_json::to_string(&report).map_err(|_| "exact_projection_report_failed")?
            );
        }
        Command::BackfillFailedRequestCosts {
            apply,
            batch_size,
            from_created_at,
            to_created_at,
            confirmed_legacy_null_request_ids,
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
                    from_created_at,
                    to_created_at,
                    confirmed_legacy_null_request_ids,
                    after,
                })
                .await
                .map_err(|_| "failed_request_cost_backfill_failed")?;
            let output = serde_json::to_string(&report)
                .map_err(|_| "failed_request_cost_backfill_report_failed")?;
            println!("{output}");
        }
        Command::Serve { role } => {
            let address: SocketAddr = config
                .listen
                .parse()
                .map_err(|_| "listen_address_invalid")?;
            let listener = TcpListener::bind(address)
                .await
                .map_err(|_| "listen_bind_failed")?;
            let (app, startup) = api::startup::router(role);
            let (worker_shutdown, mut receiver) = watch::channel(false);
            let (oauth_shutdown, oauth_receiver) = watch::channel(false);
            let proxy_state = std::sync::Arc::new(std::sync::OnceLock::new());
            let initialized_proxy_state = proxy_state.clone();
            // Keep dependency initialization off the HTTP lifecycle. A database
            // outage retains the existing static shell and explicit 503 API
            // errors, then recovers through the original router/auth chain.
            let worker_task = tokio::spawn(async move {
                loop {
                    let initialized = tokio::select! {
                        _ = receiver.changed() => return,
                        result = AppState::initialize(config.clone()) => result,
                    };
                    match initialized {
                        Ok(state) => {
                            let _ = initialized_proxy_state.set(state.clone());
                            startup.install(api::router_for_role(state.clone(), role));
                            #[cfg(feature = "experimental-plugin-revisions")]
                            let refresh = async {
                                let mut ticks =
                                    tokio::time::interval(std::time::Duration::from_secs(5));
                                ticks.set_missed_tick_behavior(
                                    tokio::time::MissedTickBehavior::Skip,
                                );
                                loop {
                                    ticks.tick().await;
                                    let _ = state.refresh_application_plugins().await;
                                }
                            };
                            let ordinary = async {
                                if role.runs_worker() {
                                    worker::run_until_shutdown_with_oauth_stop(
                                        state.clone(),
                                        receiver,
                                        oauth_receiver,
                                    )
                                    .await;
                                } else {
                                    let _ = receiver.changed().await;
                                }
                            };
                            #[cfg(feature = "experimental-plugin-revisions")]
                            tokio::select! {
                                _ = ordinary => {},
                                _ = refresh => {},
                            }
                            #[cfg(not(feature = "experimental-plugin-revisions"))]
                            ordinary.await;
                            return;
                        }
                        Err(error) => {
                            startup.failed(role, &error);
                            tokio::select! {
                                _ = receiver.changed() => return,
                                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {},
                            }
                        }
                    }
                }
            });
            info!(%address, ?role, "token center static shell listening; dependencies initialize separately");
            serve_with_worker_shutdown(
                listener,
                app,
                shutdown_signal(),
                Some(worker_task),
                Some(worker_shutdown),
                Some(oauth_shutdown),
                async move {
                    if let Some(state) = proxy_state.get() {
                        state.drain_proxy_tasks().await;
                    }
                },
            )
            .await?;
        }
    }

    Ok(())
}

async fn serve_with_worker_shutdown(
    listener: TcpListener,
    app: axum::Router,
    signal: impl std::future::Future<Output = ()>,
    mut worker_task: Option<tokio::task::JoinHandle<()>>,
    worker_shutdown: Option<watch::Sender<bool>>,
    oauth_shutdown: Option<watch::Sender<bool>>,
    proxy_drain: impl std::future::Future<Output = ()>,
) -> Result<(), &'static str> {
    let mut worker_failed = false;
    let result = memeloop_token_center::server::serve(listener, app, async {
        worker_failed = wait_for_server_shutdown(signal, &mut worker_task).await;
        if let Some(sender) = &oauth_shutdown {
            let _ = sender.send(true);
        }
    })
    .await;
    proxy_drain.await;
    if let Some(sender) = oauth_shutdown {
        let _ = sender.send(true);
    }
    if let Some(sender) = worker_shutdown {
        let _ = sender.send(true);
    }
    if let Some(task) = worker_task {
        worker_failed |= task.await.is_err();
    }
    if worker_failed {
        return Err("worker_task_failed");
    }
    result.map_err(|_| "http_server_failed")
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
#[path = "shutdown_durability_tests.rs"]
mod shutdown_durability_tests;

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[tokio::test]
    async fn all_mode_starts_oauth_drain_before_long_http_drain_and_joins_both() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use std::time::Duration;
        use tokio::sync::{Notify, oneshot};

        let http_started = Arc::new(Notify::new());
        let release_http = Arc::new(Notify::new());
        let app = axum::Router::new().route(
            "/held",
            axum::routing::get({
                let http_started = http_started.clone();
                let release_http = release_http.clone();
                move || {
                    let http_started = http_started.clone();
                    let release_http = release_http.clone();
                    async move {
                        http_started.notify_one();
                        release_http.notified().await;
                        "complete HTTP result"
                    }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (worker_stop, mut worker_shutdown) = watch::channel(false);
        let (oauth_stop, mut oauth_shutdown) = watch::channel(false);
        let mut observed_oauth_shutdown = oauth_shutdown.clone();
        let ordinary_stopped = Arc::new(AtomicUsize::new(0));
        let saved = Arc::new(AtomicUsize::new(0));
        let exchanges = Arc::new(AtomicUsize::new(0));
        let (begin_exchange, exchange_ready) = oneshot::channel();
        let (exchange_started, observed_exchange) = oneshot::channel();
        let worker = tokio::spawn({
            let ordinary_stopped = ordinary_stopped.clone();
            let saved = saved.clone();
            let exchanges = exchanges.clone();
            async move {
                let refresh = async {
                    exchange_ready.await.unwrap();
                    let mut started = Some(exchange_started);
                    let mut ticks = tokio::time::interval(Duration::from_secs(60));
                    loop {
                        tokio::select! {
                            biased;
                            _ = oauth_shutdown.changed() => break,
                            _ = ticks.tick() => {
                                exchanges.fetch_add(1, Ordering::SeqCst);
                                if let Some(started) = started.take() {
                                    started.send(()).unwrap();
                                }
                                tokio::time::sleep(Duration::from_secs(120)).await;
                                saved.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    }
                };
                let ordinary = async {
                    worker_shutdown.changed().await.unwrap();
                    ordinary_stopped.fetch_add(1, Ordering::SeqCst);
                };
                tokio::join!(refresh, ordinary);
            }
        });
        let (terminate, termination) = oneshot::channel();
        let process = tokio::spawn(serve_with_worker_shutdown(
            listener,
            app,
            async { termination.await.unwrap() },
            Some(worker),
            Some(worker_stop),
            Some(oauth_stop),
            async {},
        ));
        let request = tokio::spawn(async move {
            reqwest::Client::builder()
                .no_proxy()
                .http1_only()
                .build()
                .unwrap()
                .get(format!("http://{address}/held"))
                .send()
                .await
                .unwrap()
        });
        http_started.notified().await;
        tokio::time::pause();
        begin_exchange.send(()).unwrap();
        observed_exchange.await.unwrap();
        let started = tokio::time::Instant::now();
        terminate.send(()).unwrap();
        observed_oauth_shutdown.changed().await.unwrap();
        assert!(*observed_oauth_shutdown.borrow());
        tokio::time::advance(Duration::from_secs(121)).await;
        tokio::task::yield_now().await;
        assert_eq!(saved.load(Ordering::SeqCst), 1);
        assert_eq!(exchanges.load(Ordering::SeqCst), 1);
        assert_eq!(ordinary_stopped.load(Ordering::SeqCst), 0);
        assert!(!process.is_finished());
        assert!(!request.is_finished());
        tokio::time::advance(Duration::from_secs(1139)).await;
        assert_eq!(started.elapsed(), Duration::from_secs(1260));
        assert_eq!(exchanges.load(Ordering::SeqCst), 1);
        assert_eq!(ordinary_stopped.load(Ordering::SeqCst), 0);
        release_http.notify_one();
        tokio::time::resume();
        let response = request.await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "complete HTTP result");
        assert_eq!(process.await.unwrap(), Ok(()));
        assert_eq!(ordinary_stopped.load(Ordering::SeqCst), 1);
        assert_eq!(saved.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn exact_projection_backfill_requires_manifest_and_defaults_to_preview() {
        assert!(
            Cli::try_parse_from([
                "memeloop-token-center",
                "backfill-exact-failed-request-costs",
            ])
            .is_err()
        );
        let parsed = Cli::try_parse_from([
            "memeloop-token-center",
            "backfill-exact-failed-request-costs",
            "--manifest",
            "reviewed.json",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Command::BackfillExactFailedRequestCosts {
                apply: false,
                cost_only: false,
                ..
            }
        ));
        let cost_only = Cli::try_parse_from([
            "memeloop-token-center",
            "backfill-exact-failed-request-costs",
            "--manifest",
            "reviewed.json",
            "--cost-only",
        ])
        .unwrap();
        assert!(matches!(
            cost_only.command,
            Command::BackfillExactFailedRequestCosts {
                apply: false,
                cost_only: true,
                ..
            }
        ));
        for disallowed in ["--batch-size", "--from-created-at", "--after-created-at"] {
            assert!(
                Cli::try_parse_from([
                    "memeloop-token-center",
                    "backfill-exact-failed-request-costs",
                    "--manifest",
                    "reviewed.json",
                    disallowed,
                    "1",
                ])
                .is_err()
            );
        }
    }

    #[test]
    fn failed_request_cost_backfill_is_explicit_bounded_and_dry_run_by_default() {
        assert!(
            Cli::try_parse_from(["memeloop-token-center", "backfill-failed-request-costs"])
                .is_err()
        );
        let parsed = Cli::try_parse_from([
            "memeloop-token-center",
            "backfill-failed-request-costs",
            "--from-created-at",
            "1",
            "--to-created-at",
            "2",
        ])
        .unwrap();
        let Command::BackfillFailedRequestCosts {
            apply,
            batch_size,
            from_created_at,
            to_created_at,
            confirmed_legacy_null_request_ids,
            after_created_at,
            after_request_id,
        } = parsed.command
        else {
            panic!("expected failed request cost backfill command");
        };
        assert!(!apply);
        assert_eq!(batch_size, 100);
        assert_eq!(from_created_at, 1);
        assert_eq!(to_created_at, 2);
        assert!(confirmed_legacy_null_request_ids.is_empty());
        assert_eq!(after_created_at, None);
        assert_eq!(after_request_id, None);

        assert!(
            Cli::try_parse_from([
                "memeloop-token-center",
                "backfill-failed-request-costs",
                "--from-created-at",
                "1",
                "--to-created-at",
                "2",
                "--after-created-at",
                "1",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "memeloop-token-center",
                "backfill-failed-request-costs",
                "--from-created-at",
                "1",
                "--to-created-at",
                "2",
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
