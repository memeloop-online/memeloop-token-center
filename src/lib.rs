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
const PROXY_LIFECYCLE_CONCURRENCY: usize = 16;

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
    pub(crate) proxy_lifecycle_permits: Arc<tokio::sync::Semaphore>,
}

impl AppState {
    pub async fn initialize(config: Config) -> anyhow_free::Result<Self> {
        let db = Database::connect_with_max(&config.database_url, config.database_max_connections)
            .await?;
        if config.run_migrations_on_start {
            db.migrate().await?;
        }
        let archive = ArchiveStore::from_config(&config).await?;
        let plugins = PluginRuntime::load(config.plugin_dir.as_deref(), db.clone())?;
        plugins.validate_stored_configurations().await?;
        let mut providers = ProviderCatalog::builtins();
        providers.extend(plugins.provider_types())?;

        Ok(Self {
            config: Arc::new(config),
            db,
            archive,
            providers,
            plugins,
            metrics: metrics::Metrics::default(),
            request_event_streams: request_event_stream::RequestEventStreamLimiter::default(),
            proxy_lifecycle_permits: Arc::new(tokio::sync::Semaphore::new(
                PROXY_LIFECYCLE_CONCURRENCY,
            )),
            http: build_http_client()?,
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

mod anyhow_free {
    pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, path},
    };

    use super::{build_http_client, build_pinned_http_client};

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
}
