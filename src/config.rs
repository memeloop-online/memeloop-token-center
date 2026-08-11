use std::env;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub listen: String,
    pub database_url: String,
    pub key_pepper: String,
    pub service_token: String,
    pub archive_backend: ArchiveBackend,
    pub archive_path: Option<String>,
    pub s3_bucket: Option<String>,
    pub s3_endpoint: Option<String>,
    pub s3_region: String,
    pub s3_access_key: Option<String>,
    pub s3_secret_key: Option<String>,
    pub s3_allow_http: bool,
    pub upstream_openai_url: Option<String>,
    pub upstream_openai_key: Option<String>,
    pub upstream_anthropic_url: Option<String>,
    pub upstream_anthropic_key: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveBackend {
    S3,
    Filesystem,
    Memory,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let archive_backend = match env_string("MTC_ARCHIVE_BACKEND", "s3").as_str() {
            "s3" => ArchiveBackend::S3,
            "filesystem" => ArchiveBackend::Filesystem,
            "memory" => ArchiveBackend::Memory,
            value => return Err(ConfigError::InvalidArchiveBackend(value.to_owned())),
        };

        let key_pepper = required("MTC_KEY_PEPPER")?;
        if key_pepper.len() < 32 {
            return Err(ConfigError::WeakKeyPepper);
        }

        Ok(Self {
            listen: env_string("MTC_LISTEN", "0.0.0.0:8080"),
            database_url: env_string(
                "MTC_DATABASE_URL",
                "postgres://postgres:postgres@127.0.0.1:5432/memeloop_token_center",
            ),
            key_pepper,
            service_token: required("MTC_SERVICE_TOKEN")?,
            archive_backend,
            archive_path: env::var("MTC_ARCHIVE_PATH").ok(),
            s3_bucket: env::var("MTC_S3_BUCKET").ok(),
            s3_endpoint: env::var("MTC_S3_ENDPOINT").ok(),
            s3_region: env_string("MTC_S3_REGION", "us-east-1"),
            s3_access_key: env::var("MTC_S3_ACCESS_KEY").ok(),
            s3_secret_key: env::var("MTC_S3_SECRET_KEY").ok(),
            s3_allow_http: env_bool("MTC_S3_ALLOW_HTTP", false),
            upstream_openai_url: env::var("MTC_UPSTREAM_OPENAI_URL").ok(),
            upstream_openai_key: env::var("MTC_UPSTREAM_OPENAI_KEY").ok(),
            upstream_anthropic_url: env::var("MTC_UPSTREAM_ANTHROPIC_URL").ok(),
            upstream_anthropic_key: env::var("MTC_UPSTREAM_ANTHROPIC_KEY").ok(),
        })
    }

    pub fn for_test(database_url: String) -> Self {
        Self {
            listen: "127.0.0.1:0".to_owned(),
            database_url,
            key_pepper: "test-pepper-must-have-at-least-32-bytes".to_owned(),
            service_token: "test-service-token".to_owned(),
            archive_backend: ArchiveBackend::Memory,
            archive_path: None,
            s3_bucket: None,
            s3_endpoint: None,
            s3_region: "us-east-1".to_owned(),
            s3_access_key: None,
            s3_secret_key: None,
            s3_allow_http: true,
            upstream_openai_url: None,
            upstream_openai_key: None,
            upstream_anthropic_url: None,
            upstream_anthropic_key: None,
        }
    }
}

fn required(name: &'static str) -> Result<String, ConfigError> {
    env::var(name).map_err(|_| ConfigError::Missing(name))
}

fn env_string(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn env_bool(name: &str, default: bool) -> bool {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("missing required environment variable {0}")]
    Missing(&'static str),
    #[error("MTC_KEY_PEPPER must contain at least 32 bytes")]
    WeakKeyPepper,
    #[error("unsupported archive backend: {0}")]
    InvalidArchiveBackend(String),
}
