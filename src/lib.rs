pub mod api;
pub mod archive;
pub mod config;
pub mod conversation;
pub mod crypto;
pub mod db;
pub mod error;
pub mod model;

use std::sync::Arc;

use archive::ArchiveStore;
use config::Config;
use db::Database;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Database,
    pub archive: ArchiveStore,
    pub http: reqwest::Client,
}

impl AppState {
    pub async fn initialize(config: Config) -> anyhow_free::Result<Self> {
        let db = Database::connect(&config.database_url).await?;
        db.migrate().await?;
        let archive = ArchiveStore::from_config(&config).await?;

        Ok(Self {
            config: Arc::new(config),
            db,
            archive,
            http: reqwest::Client::builder()
                .pool_max_idle_per_host(64)
                .build()?,
        })
    }
}

mod anyhow_free {
    pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
}
