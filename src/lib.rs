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
pub mod provider;
pub mod worker;

use std::sync::Arc;

use archive::ArchiveStore;
use config::Config;
use db::Database;
use plugin::PluginRuntime;
use provider::ProviderCatalog;

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
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .pool_max_idle_per_host(8)
                .pool_idle_timeout(std::time::Duration::from_secs(30))
                .build()?,
        })
    }
}

mod anyhow_free {
    pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
}
