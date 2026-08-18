use std::env;

use serde::{Deserialize, Serialize};

pub const DEFAULT_PRICING_MODELS_DEV_URL: &str = "https://models.dev/catalog.json";
pub const DEFAULT_PRICING_LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
pub const DEFAULT_PRICING_OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/models";

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeRole {
    Gateway,
    Control,
    Worker,
    #[default]
    All,
}

impl RuntimeRole {
    pub fn serves_gateway(self) -> bool {
        matches!(self, Self::Gateway | Self::All)
    }

    pub fn serves_control(self) -> bool {
        matches!(self, Self::Control | Self::All)
    }

    pub fn runs_worker(self) -> bool {
        matches!(self, Self::Worker | Self::All)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub listen: String,
    pub database_url: String,
    pub database_max_connections: u32,
    pub run_migrations_on_start: bool,
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
    pub pricing_models_dev_url: String,
    pub pricing_litellm_url: String,
    pub pricing_openrouter_url: String,
    pub plugin_dir: Option<String>,
    pub allow_oauth_loopback: bool,
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

        let service_token = required("MTC_SERVICE_TOKEN")?;
        validate_bootstrap_service_token(&service_token)?;

        let allow_oauth_loopback = env_bool("MTC_ALLOW_OAUTH_LOOPBACK", false);
        let pricing_models_dev_url = pricing_source_url(
            "MTC_PRICING_MODELS_DEV_URL",
            env_string("MTC_PRICING_MODELS_DEV_URL", DEFAULT_PRICING_MODELS_DEV_URL),
            allow_oauth_loopback,
        )?;
        let pricing_litellm_url = pricing_source_url(
            "MTC_PRICING_LITELLM_URL",
            env_string("MTC_PRICING_LITELLM_URL", DEFAULT_PRICING_LITELLM_URL),
            allow_oauth_loopback,
        )?;
        let pricing_openrouter_url = pricing_source_url(
            "MTC_PRICING_OPENROUTER_URL",
            env_string("MTC_PRICING_OPENROUTER_URL", DEFAULT_PRICING_OPENROUTER_URL),
            allow_oauth_loopback,
        )?;

        Ok(Self {
            listen: env_string("MTC_LISTEN", "0.0.0.0:8080"),
            database_url: env_string(
                "MTC_DATABASE_URL",
                "postgres://postgres:postgres@127.0.0.1:5432/memeloop_token_center",
            ),
            database_max_connections: env_u32("MTC_DATABASE_MAX_CONNECTIONS", 4)?.clamp(1, 32),
            run_migrations_on_start: env_bool("MTC_RUN_MIGRATIONS_ON_START", true),
            key_pepper,
            service_token,
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
            pricing_models_dev_url,
            pricing_litellm_url,
            pricing_openrouter_url,
            plugin_dir: env::var("MTC_PLUGIN_DIR").ok(),
            allow_oauth_loopback,
        })
    }

    /// Minimal configuration for the one-shot session archive importer.
    ///
    /// It deliberately does not read control-plane credentials, upstream
    /// credentials, plugin settings or pricing-source overrides. The importer
    /// needs only the target database and archive object store.
    pub fn from_session_archive_import_env() -> Result<Self, ConfigError> {
        let archive_backend = match env_string("MTC_ARCHIVE_BACKEND", "s3").as_str() {
            "s3" => ArchiveBackend::S3,
            "filesystem" => ArchiveBackend::Filesystem,
            "memory" => ArchiveBackend::Memory,
            value => return Err(ConfigError::InvalidArchiveBackend(value.to_owned())),
        };
        Ok(Self {
            listen: "127.0.0.1:0".to_owned(),
            database_url: required("MTC_DATABASE_URL")?,
            database_max_connections: 2,
            run_migrations_on_start: false,
            key_pepper: "unused-by-session-archive-importer".to_owned(),
            service_token: "unused-by-session-archive-importer".to_owned(),
            archive_backend,
            archive_path: env::var("MTC_ARCHIVE_PATH").ok(),
            s3_bucket: env::var("MTC_S3_BUCKET").ok(),
            s3_endpoint: env::var("MTC_S3_ENDPOINT").ok(),
            s3_region: env_string("MTC_S3_REGION", "us-east-1"),
            s3_access_key: env::var("MTC_S3_ACCESS_KEY").ok(),
            s3_secret_key: env::var("MTC_S3_SECRET_KEY").ok(),
            s3_allow_http: env_bool("MTC_S3_ALLOW_HTTP", false),
            upstream_openai_url: None,
            upstream_openai_key: None,
            upstream_anthropic_url: None,
            upstream_anthropic_key: None,
            pricing_models_dev_url: DEFAULT_PRICING_MODELS_DEV_URL.to_owned(),
            pricing_litellm_url: DEFAULT_PRICING_LITELLM_URL.to_owned(),
            pricing_openrouter_url: DEFAULT_PRICING_OPENROUTER_URL.to_owned(),
            plugin_dir: None,
            allow_oauth_loopback: false,
        })
    }

    pub fn for_test(database_url: String) -> Self {
        Self {
            listen: "127.0.0.1:0".to_owned(),
            database_url,
            database_max_connections: 8,
            run_migrations_on_start: true,
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
            pricing_models_dev_url: DEFAULT_PRICING_MODELS_DEV_URL.to_owned(),
            pricing_litellm_url: DEFAULT_PRICING_LITELLM_URL.to_owned(),
            pricing_openrouter_url: DEFAULT_PRICING_OPENROUTER_URL.to_owned(),
            plugin_dir: None,
            allow_oauth_loopback: true,
        }
    }
}

fn pricing_source_url(
    name: &'static str,
    value: String,
    allow_test_loopback: bool,
) -> Result<String, ConfigError> {
    let url = url::Url::parse(&value).map_err(|_| ConfigError::InvalidPricingSourceUrl(name))?;
    let host = url
        .host_str()
        .ok_or(ConfigError::InvalidPricingSourceUrl(name))?;
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host.to_ascii_lowercase().ends_with(".localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    let allowed_scheme =
        url.scheme() == "https" || (allow_test_loopback && loopback && url.scheme() == "http");
    if !allowed_scheme
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigError::InvalidPricingSourceUrl(name));
    }
    Ok(value)
}

fn validate_bootstrap_service_token(value: &str) -> Result<(), ConfigError> {
    if value.len() < 32 {
        return Err(ConfigError::ServiceTokenTooShort);
    }
    if value
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(ConfigError::InvalidServiceTokenCharacters);
    }
    Ok(())
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

fn env_u32(name: &'static str, default: u32) -> Result<u32, ConfigError> {
    match env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|_| ConfigError::InvalidInteger(name, value)),
        Err(_) => Ok(default),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("missing required environment variable {0}")]
    Missing(&'static str),
    #[error("MTC_KEY_PEPPER must contain at least 32 bytes")]
    WeakKeyPepper,
    #[error("MTC_SERVICE_TOKEN must contain at least 32 bytes")]
    ServiceTokenTooShort,
    #[error("MTC_SERVICE_TOKEN must not contain any whitespace or control characters")]
    InvalidServiceTokenCharacters,
    #[error("unsupported archive backend: {0}")]
    InvalidArchiveBackend(String),
    #[error("{0} must be an unsigned integer, received {1}")]
    InvalidInteger(&'static str, String),
    #[error("{0} must be a credential-free public HTTPS URL")]
    InvalidPricingSourceUrl(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_service_token_requires_at_least_32_bytes() {
        for value in [String::new(), "a".repeat(31)] {
            assert!(matches!(
                validate_bootstrap_service_token(&value),
                Err(ConfigError::ServiceTokenTooShort)
            ));
        }
        assert!(validate_bootstrap_service_token(&"a".repeat(32)).is_ok());
        assert!(validate_bootstrap_service_token(&"é".repeat(16)).is_ok());
    }

    #[test]
    fn bootstrap_service_token_rejects_whitespace_and_control_characters_anywhere() {
        for value in [
            format!(" {}", "a".repeat(32)),
            format!("{} ", "a".repeat(32)),
            format!("{}\t{}", "a".repeat(16), "b".repeat(16)),
            format!("{}\u{2003}{}", "a".repeat(16), "b".repeat(16)),
            format!("{}\u{001b}{}", "a".repeat(16), "b".repeat(16)),
        ] {
            assert!(matches!(
                validate_bootstrap_service_token(&value),
                Err(ConfigError::InvalidServiceTokenCharacters)
            ));
        }
    }

    #[test]
    fn pricing_sources_require_credential_free_https_except_explicit_test_loopback() {
        assert!(
            pricing_source_url(
                "SOURCE",
                "https://models.example.test/catalog.json".to_owned(),
                false,
            )
            .is_ok()
        );
        for value in [
            "http://models.example.test/catalog.json",
            "https://user:secret@models.example.test/catalog.json",
            "https://models.example.test/catalog.json#fragment",
            "file:///tmp/catalog.json",
        ] {
            assert!(
                pricing_source_url("SOURCE", value.to_owned(), false).is_err(),
                "{value}"
            );
        }
        assert!(
            pricing_source_url("SOURCE", "http://127.0.0.1:1234/catalog".to_owned(), false)
                .is_err()
        );
        assert!(
            pricing_source_url("SOURCE", "http://127.0.0.1:1234/catalog".to_owned(), true).is_ok()
        );
        assert!(
            pricing_source_url("SOURCE", "http://10.0.0.1:1234/catalog".to_owned(), true,).is_err()
        );
    }
}
