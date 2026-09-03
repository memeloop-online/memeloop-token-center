pub mod api;
pub mod archive;
pub mod archive_reaper;
pub mod archive_staging;
pub mod config;
pub mod conversation;
pub mod crypto;
pub mod db;
pub mod error;
mod gateway_body;
pub mod generation;
#[cfg(not(target_env = "msvc"))]
mod jemalloc_control;
pub mod metrics;
pub mod model;
pub mod network;
pub mod oauth;
pub mod plugin;
#[cfg(feature = "plugin-distribution")]
pub mod plugin_distribution;
pub mod pricing;
pub mod provider;
mod proxy_lifecycle;
mod request_event_stream;
pub mod schema;
pub mod server;
pub mod session_archive_import;
pub mod worker;

use std::{sync::Arc, time::Duration};

use archive::ArchiveStore;
use config::Config;
use db::Database;
use plugin::PluginRuntime;
use provider::ProviderCatalog;

const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
// Synchronous image providers may legitimately stay silent while generating.
// The image path has its own 2-request concurrency and 16 MiB body bounds, so
// matching the read timeout to the overall deadline does not make it unbounded.
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(600);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(21 * 60);
// Each streaming response archive owns a 5 MiB multipart buffer. Keep the
// upstream/request concurrency independent, but bound simultaneous archive
// writers so one gateway cannot multiply that buffer by all active lifecycles.
const PROXY_ARCHIVE_STREAM_CONCURRENCY: usize = 4;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Database,
    pub archive: ArchiveStore,
    pub http: reqwest::Client,
    pub providers: ProviderCatalog,
    pub plugins: PluginRuntime,
    pub metrics: metrics::Metrics,
    pub(crate) request_event_streams: request_event_stream::RequestEventStreamLimiter,
    pub(crate) gateway_body_read_permits: Arc<tokio::sync::Semaphore>,
    pub(crate) proxy_lifecycle_permits: Arc<tokio::sync::Semaphore>,
    pub(crate) proxy_archive_stream_permits: Arc<tokio::sync::Semaphore>,
}

#[derive(Debug, thiserror::Error)]
pub enum InitializationError {
    #[error("database initialization failed")]
    Database,
    #[error("archive initialization failed")]
    Archive,
    #[error("plugin initialization failed")]
    Plugin,
    #[error("HTTP client initialization failed")]
    HttpClient,
}

impl AppState {
    pub async fn initialize(config: Config) -> Result<Self, InitializationError> {
        let proxy_lifecycle_concurrency = config.proxy_lifecycle_concurrency as usize;
        let gateway_body_read_concurrency = config.gateway_body_read_concurrency as usize;
        if config.run_migrations_on_start {
            let migration_db = Database::connect_for_migration(
                &config.database_url,
                config.database_max_connections,
            )
            .await
            .map_err(|_| InitializationError::Database)?;
            migration_db
                .migrate()
                .await
                .map_err(|_| InitializationError::Database)?;
            migration_db.close().await;
        }
        let db = Database::connect_with_max(&config.database_url, config.database_max_connections)
            .await
            .map_err(|_| InitializationError::Database)?;
        let archive = ArchiveStore::from_config(&config)
            .await
            .map_err(|_| InitializationError::Archive)?;
        let plugins = PluginRuntime::load(config.plugin_dir.as_deref(), db.clone())
            .map_err(|_| InitializationError::Plugin)?;
        plugins
            .validate_stored_configurations()
            .await
            .map_err(|_| InitializationError::Plugin)?;
        let mut providers = ProviderCatalog::builtins();
        providers
            .extend(plugins.provider_types())
            .map_err(|_| InitializationError::Plugin)?;

        Ok(Self {
            config: Arc::new(config),
            db,
            archive,
            providers,
            plugins,
            metrics: metrics::Metrics::default(),
            request_event_streams: request_event_stream::RequestEventStreamLimiter::default(),
            gateway_body_read_permits: Arc::new(tokio::sync::Semaphore::new(
                gateway_body_read_concurrency,
            )),
            proxy_lifecycle_permits: Arc::new(tokio::sync::Semaphore::new(
                proxy_lifecycle_concurrency,
            )),
            proxy_archive_stream_permits: Arc::new(tokio::sync::Semaphore::new(
                PROXY_ARCHIVE_STREAM_CONCURRENCY,
            )),
            http: build_http_client().map_err(|_| InitializationError::HttpClient)?,
        })
    }
}

fn base_http_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .read_timeout(HTTP_READ_TIMEOUT)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        // An inherited proxy would perform its own DNS lookup and bypass
        // resolve_to_addrs pinning. A future trusted proxy must be explicit.
        .no_proxy()
        .pool_max_idle_per_host(8)
        .pool_idle_timeout(Duration::from_secs(30))
}

pub(crate) fn build_http_client() -> Result<reqwest::Client, reqwest::Error> {
    base_http_client_builder().build()
}

pub(crate) fn build_pinned_http_client(
    hostname: &str,
    addresses: &[std::net::SocketAddr],
) -> Result<reqwest::Client, reqwest::Error> {
    base_http_client_builder()
        // These clients live for one outbound operation. Do not retain a pool
        // for arbitrary provider hostnames after the operation is dropped.
        .pool_max_idle_per_host(0)
        .resolve_to_addrs(hostname, addresses)
        .build()
}

pub(crate) fn build_explicit_proxy_http_client(
    proxy_url: &str,
    pinned_hosts: &[(&str, &[std::net::SocketAddr])],
) -> Result<reqwest::Client, reqwest::Error> {
    let mut builder = base_http_client_builder()
        .pool_max_idle_per_host(0)
        .proxy(reqwest::Proxy::all(proxy_url)?);
    for (hostname, addresses) in pinned_hosts {
        builder = builder.resolve_to_addrs(hostname, addresses);
    }
    builder.build()
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, SocketAddr};

    use reqwest::StatusCode;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, path},
    };

    use super::{build_explicit_proxy_http_client, build_http_client, build_pinned_http_client};

    #[tokio::test]
    async fn shared_http_client_does_not_follow_redirects() {
        let server = MockServer::start().await;
        Mock::given(path("/redirect"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/target"))
            .mount(&server)
            .await;

        let response = build_http_client()
            .expect("shared HTTP client")
            .get(format!("{}/redirect", server.uri()))
            .send()
            .await
            .expect("redirect response");

        assert_eq!(response.status(), StatusCode::FOUND);
    }

    #[tokio::test]
    async fn pinned_client_keeps_the_original_http_host() {
        let server = MockServer::start().await;
        let expected_host = format!("pin-test.invalid:{}", server.address().port());
        Mock::given(path("/probe"))
            .and(header("host", expected_host))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_pinned_http_client("pin-test.invalid", &[*server.address()])
            .expect("pinned HTTP client");
        let response = client
            .get(format!(
                "http://pin-test.invalid:{}/probe",
                server.address().port()
            ))
            .send()
            .await
            .expect("pinned request");

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn socks5_proxy_receives_the_locally_pinned_target_address() {
        let target = MockServer::start().await;
        let expected_host = format!("pin-target.invalid:{}", target.address().port());
        Mock::given(path("/through-proxy"))
            .and(header("host", expected_host))
            .respond_with(ResponseTemplate::new(204).insert_header("connection", "close"))
            .expect(1)
            .mount(&target)
            .await;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = listener.local_addr().unwrap();
        let target_address = *target.address();
        let proxy = tokio::spawn(async move {
            let (mut client, _) = listener.accept().await.unwrap();
            let mut greeting = [0_u8; 2];
            client.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting[0], 5);
            let mut methods = vec![0_u8; usize::from(greeting[1])];
            client.read_exact(&mut methods).await.unwrap();
            assert!(methods.contains(&0));
            client.write_all(&[5, 0]).await.unwrap();

            let mut request = [0_u8; 4];
            client.read_exact(&mut request).await.unwrap();
            assert_eq!(&request[..3], &[5, 1, 0]);
            let ip = match request[3] {
                1 => {
                    let mut bytes = [0_u8; 4];
                    client.read_exact(&mut bytes).await.unwrap();
                    IpAddr::from(bytes)
                }
                4 => {
                    let mut bytes = [0_u8; 16];
                    client.read_exact(&mut bytes).await.unwrap();
                    IpAddr::from(bytes)
                }
                3 => panic!("socks5 proxy received a hostname instead of the pinned target IP"),
                value => panic!("unexpected SOCKS5 address type {value}"),
            };
            let mut port = [0_u8; 2];
            client.read_exact(&mut port).await.unwrap();
            let requested = SocketAddr::new(ip, u16::from_be_bytes(port));
            assert_eq!(requested, target_address);

            let mut upstream = TcpStream::connect(requested).await.unwrap();
            client
                .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
            tokio::io::copy_bidirectional(&mut client, &mut upstream)
                .await
                .unwrap();
        });

        let target_addresses = [*target.address()];
        let proxy_addresses = [proxy_address];
        let pins = [
            ("pin-target.invalid", &target_addresses[..]),
            ("proxy-test.invalid", &proxy_addresses[..]),
        ];
        let client = build_explicit_proxy_http_client(
            &format!("socks5://proxy-test.invalid:{}", proxy_address.port()),
            &pins,
        )
        .unwrap();
        let response = client
            .get(format!(
                "http://pin-target.invalid:{}/through-proxy",
                target.address().port()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        drop(response);
        drop(client);
        tokio::time::timeout(std::time::Duration::from_secs(2), proxy)
            .await
            .unwrap()
            .unwrap();
    }
}
