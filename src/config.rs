use std::env;

use serde::{Deserialize, Serialize};

pub const DEFAULT_PRICING_MODELS_DEV_URL: &str = "https://models.dev/catalog.json";
pub const DEFAULT_PRICING_LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
pub const DEFAULT_PRICING_OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/models";
pub const DEFAULT_GATEWAY_BODY_READ_CONCURRENCY: u32 = 1_024;
pub const MAX_GATEWAY_BODY_READ_CONCURRENCY: u32 = 8_192;

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

#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    pub listen: String,
    pub database_url: String,
    pub database_max_connections: u32,
    /// Maximum complete proxy lifecycles retained by one gateway process.
    /// Service saturation is distinct from a credential policy limit and is
    /// reported as HTTP 503, never as a per-key 429.
    pub proxy_lifecycle_concurrency: u32,
    /// Maximum gateway request bodies buffered concurrently. The permit covers
    /// only the bounded body read, not the complete proxy lifecycle.
    pub gateway_body_read_concurrency: u32,
    pub run_migrations_on_start: bool,
    pub key_pepper: String,
    pub service_token: String,
    /// Shared HMAC secret for the MemeLoop Cloud subscription webhook.  The
    /// endpoint fails closed when this integration is not configured.
    pub memeloop_cloud_webhook_secret: Option<String>,
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
    /// Registers authenticated control-plane diagnostics when explicitly set.
    /// The routes remain absent (404) by default and on gateway/worker roles.
    pub runtime_profiling_enabled: bool,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Config")
            .field("listen", &self.listen)
            .field("database_url", &"[redacted]")
            .field("database_max_connections", &self.database_max_connections)
            .field(
                "proxy_lifecycle_concurrency",
                &self.proxy_lifecycle_concurrency,
            )
            .field(
                "gateway_body_read_concurrency",
                &self.gateway_body_read_concurrency,
            )
            .field("run_migrations_on_start", &self.run_migrations_on_start)
            .field("key_pepper", &"[redacted]")
            .field("service_token", &"[redacted]")
            .field(
                "memeloop_cloud_webhook_secret",
                &self
                    .memeloop_cloud_webhook_secret
                    .as_ref()
                    .map(|_| "[redacted]"),
            )
            .field("archive_backend", &self.archive_backend)
            .field("archive_path", &self.archive_path)
            .field("s3_bucket", &self.s3_bucket)
            .field(
                "s3_endpoint",
                &self.s3_endpoint.as_ref().map(|_| "[configured]"),
            )
            .field("s3_region", &self.s3_region)
            .field(
                "s3_access_key",
                &self.s3_access_key.as_ref().map(|_| "[redacted]"),
            )
            .field(
                "s3_secret_key",
                &self.s3_secret_key.as_ref().map(|_| "[redacted]"),
            )
            .field("s3_allow_http", &self.s3_allow_http)
            .field(
                "upstream_openai_url",
                &self.upstream_openai_url.as_ref().map(|_| "[configured]"),
            )
            .field(
                "upstream_openai_key",
                &self.upstream_openai_key.as_ref().map(|_| "[redacted]"),
            )
            .field(
                "upstream_anthropic_url",
                &self.upstream_anthropic_url.as_ref().map(|_| "[configured]"),
            )
            .field(
                "upstream_anthropic_key",
                &self.upstream_anthropic_key.as_ref().map(|_| "[redacted]"),
            )
            .field("pricing_models_dev_url", &"[configured public HTTPS URL]")
            .field("pricing_litellm_url", &"[configured public HTTPS URL]")
            .field("pricing_openrouter_url", &"[configured public HTTPS URL]")
            .field("plugin_dir", &self.plugin_dir)
            .field("allow_oauth_loopback", &self.allow_oauth_loopback)
            .field("runtime_profiling_enabled", &self.runtime_profiling_enabled)
            .finish()
    }
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
        let archive_backend = production_archive_backend(&env_string("MTC_ARCHIVE_BACKEND", "s3"))?;

        let key_pepper = required("MTC_KEY_PEPPER")?;
        if key_pepper.len() < 32 {
            return Err(ConfigError::WeakKeyPepper);
        }

        let service_token = required("MTC_SERVICE_TOKEN")?;
        validate_bootstrap_service_token(&service_token)?;

        let memeloop_cloud_webhook_secret =
            optional_secret("MTC_MEMELOOP_CLOUD_WEBHOOK_SECRET", 32)?;

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
            proxy_lifecycle_concurrency: env_u32("MTC_PROXY_LIFECYCLE_CONCURRENCY", 64)?
                .clamp(1, 4_096),
            gateway_body_read_concurrency: gateway_body_read_concurrency(env_u32(
                "MTC_GATEWAY_BODY_READ_CONCURRENCY",
                DEFAULT_GATEWAY_BODY_READ_CONCURRENCY,
            )?),
            run_migrations_on_start: env_bool("MTC_RUN_MIGRATIONS_ON_START", true),
            key_pepper,
            service_token,
            memeloop_cloud_webhook_secret,
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
            runtime_profiling_enabled: env_bool("MTC_RUNTIME_PROFILING_ENABLED", false),
        })
    }

    /// Minimal configuration for the one-shot session archive importer.
    ///
    /// It deliberately does not read control-plane credentials, upstream
    /// credentials, plugin settings or pricing-source overrides. The importer
    /// needs only the target database and archive object store.
    pub fn from_session_archive_import_env() -> Result<Self, ConfigError> {
        let archive_backend = production_archive_backend(&env_string("MTC_ARCHIVE_BACKEND", "s3"))?;
        Ok(Self {
            listen: "127.0.0.1:0".to_owned(),
            database_url: required("MTC_DATABASE_URL")?,
            database_max_connections: 2,
            proxy_lifecycle_concurrency: 1,
            gateway_body_read_concurrency: 1,
            run_migrations_on_start: false,
            key_pepper: "unused-by-session-archive-importer".to_owned(),
            service_token: "unused-by-session-archive-importer".to_owned(),
            memeloop_cloud_webhook_secret: None,
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
            runtime_profiling_enabled: false,
        })
    }

    pub fn for_test(database_url: String) -> Self {
        Self {
            listen: "127.0.0.1:0".to_owned(),
            database_url,
            database_max_connections: 8,
            proxy_lifecycle_concurrency: 64,
            gateway_body_read_concurrency: DEFAULT_GATEWAY_BODY_READ_CONCURRENCY,
            run_migrations_on_start: true,
            key_pepper: "test-pepper-must-have-at-least-32-bytes".to_owned(),
            service_token: "test-service-token".to_owned(),
            memeloop_cloud_webhook_secret: Some(
                "test-memeloop-cloud-webhook-secret-long-enough".to_owned(),
            ),
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
            runtime_profiling_enabled: false,
        }
    }
}

fn production_archive_backend(value: &str) -> Result<ArchiveBackend, ConfigError> {
    match value {
        "s3" => Ok(ArchiveBackend::S3),
        "filesystem" => Ok(ArchiveBackend::Filesystem),
        // The in-memory object store has no capacity or retention bound. Keep
        // it reachable only through Config::for_test, never through a serving
        // process or one-shot importer environment.
        other => Err(ConfigError::InvalidArchiveBackend(other.to_owned())),
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

fn optional_secret(
    name: &'static str,
    minimum_bytes: usize,
) -> Result<Option<String>, ConfigError> {
    let Ok(value) = env::var(name) else {
        return Ok(None);
    };
    if value.len() < minimum_bytes {
        return Err(ConfigError::SecretTooShort(name, minimum_bytes));
    }
    if value
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(ConfigError::InvalidSecretCharacters(name));
    }
    Ok(Some(value))
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

fn gateway_body_read_concurrency(value: u32) -> u32 {
    value.clamp(1, MAX_GATEWAY_BODY_READ_CONCURRENCY)
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
    #[error("{0} must contain at least {1} bytes")]
    SecretTooShort(&'static str, usize),
    #[error("{0} must not contain whitespace or control characters")]
    InvalidSecretCharacters(&'static str),
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
    fn debug_output_never_contains_runtime_credentials_or_credential_bearing_urls() {
        let secrets = [
            "postgres-password-secret",
            "pepper-secret-material",
            "bootstrap-service-secret",
            "cloud-webhook-secret",
            "s3-access-secret",
            "s3-secret-secret",
            "openai-upstream-secret",
            "anthropic-upstream-secret",
            "url-query-secret",
        ];
        let mut config = Config::for_test(
            "postgres://operator:postgres-password-secret@db.internal/token-center".to_owned(),
        );
        config.key_pepper = "pepper-secret-material".to_owned();
        config.service_token = "bootstrap-service-secret".to_owned();
        config.memeloop_cloud_webhook_secret = Some("cloud-webhook-secret".to_owned());
        config.s3_access_key = Some("s3-access-secret".to_owned());
        config.s3_secret_key = Some("s3-secret-secret".to_owned());
        config.s3_endpoint = Some("https://s3.example.test?signature=url-query-secret".to_owned());
        config.upstream_openai_url =
            Some("https://api.example.test/v1?token=url-query-secret".to_owned());
        config.upstream_openai_key = Some("openai-upstream-secret".to_owned());
        config.upstream_anthropic_url =
            Some("https://anthropic.example.test?token=url-query-secret".to_owned());
        config.upstream_anthropic_key = Some("anthropic-upstream-secret".to_owned());

        let debug = format!("{config:?}");
        for secret in secrets {
            assert!(!debug.contains(secret), "debug output leaked {secret}");
        }
        assert!(debug.contains("database_max_connections"));
        assert!(debug.contains("[redacted]"));
    }

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

    #[test]
    fn production_configuration_rejects_the_unbounded_memory_archive() {
        assert_eq!(
            production_archive_backend("s3").unwrap(),
            ArchiveBackend::S3
        );
        assert_eq!(
            production_archive_backend("filesystem").unwrap(),
            ArchiveBackend::Filesystem
        );
        assert!(matches!(
            production_archive_backend("memory"),
            Err(ConfigError::InvalidArchiveBackend(value)) if value == "memory"
        ));
        assert_eq!(
            Config::for_test("sqlite::memory:".to_owned()).archive_backend,
            ArchiveBackend::Memory
        );
    }

    #[test]
    fn gateway_body_read_concurrency_defaults_to_a_thousand_and_clamps() {
        assert_eq!(DEFAULT_GATEWAY_BODY_READ_CONCURRENCY, 1_024);
        assert_eq!(gateway_body_read_concurrency(0), 1);
        assert_eq!(gateway_body_read_concurrency(1_024), 1_024);
        assert_eq!(
            gateway_body_read_concurrency(MAX_GATEWAY_BODY_READ_CONCURRENCY + 1),
            MAX_GATEWAY_BODY_READ_CONCURRENCY
        );
        assert_eq!(
            Config::for_test("sqlite::memory:".to_owned()).gateway_body_read_concurrency,
            DEFAULT_GATEWAY_BODY_READ_CONCURRENCY
        );
    }
}
