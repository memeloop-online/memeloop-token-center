pub mod api;
pub mod archive;
pub mod config;
pub mod conversation;
pub mod crypto;
pub mod db;
pub mod error;
pub mod generation;
pub mod model;
pub mod oauth;
pub mod plugin;
pub mod pricing;
pub mod provider;
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
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Database,
    pub archive: ArchiveStore,
    pub http: reqwest::Client,
    pub providers: ProviderCatalog,
    pub plugins: PluginRuntime,
}

impl AppState {
    pub async fn initialize(config: Config) -> anyhow_free::Result<Self> {
        let db = Database::connect(&config.database_url).await?;
        db.migrate().await?;
        let archive = ArchiveStore::from_config(&config).await?;
        let plugins = PluginRuntime::load(config.plugin_dir.as_deref(), db.clone())?;
        let mut providers = ProviderCatalog::builtins();
        providers.extend(plugins.provider_types())?;

        Ok(Self {
            config: Arc::new(config),
            db,
            archive,
            providers,
            plugins,
            http: build_http_client()?,
        })
    }
}

fn build_http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .read_timeout(HTTP_READ_TIMEOUT)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .pool_max_idle_per_host(8)
        .pool_idle_timeout(Duration::from_secs(30))
        .build()
}

mod anyhow_free {
    pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

    use super::build_http_client;

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
}
