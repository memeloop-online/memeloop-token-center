//! Offline/background lifecycle for independently disposable RustFS copies.

use crate::{
    archive::{ArchiveStore, CasArchiveObject},
    config::{ArchiveBackend, Config},
};
use clap::Subcommand;
use uuid::Uuid;

#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Copy a verified durable original into the independent cache.
    Publish {
        #[arg(long)]
        tenant: Uuid,
        #[arg(long)]
        digest: String,
        #[arg(long)]
        bytes: u64,
        #[arg(long)]
        compressed: bool,
    },
    /// Inspect or evict the oldest completed duplicate originals.
    Reap {
        #[arg(long)]
        high_bytes: u64,
        #[arg(long)]
        low_bytes: u64,
        #[arg(long)]
        apply: bool,
    },
}

fn config(prefix: &str) -> Result<Config, &'static str> {
    let required = |name: &str| {
        std::env::var(format!("{prefix}_{name}"))
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or("missing storage configuration")
    };
    // No database is opened by this command.
    let mut config = Config::for_test("sqlite::memory:".into());
    config.archive_backend = ArchiveBackend::S3;
    config.s3_endpoint = Some(required("ENDPOINT")?);
    config.s3_bucket = Some(required("BUCKET")?);
    config.s3_access_key = Some(required("ACCESS_KEY")?);
    config.s3_secret_key = Some(required("SECRET_KEY")?);
    config.s3_region =
        std::env::var(format!("{prefix}_REGION")).unwrap_or_else(|_| "us-east-1".into());
    config.s3_allow_http = std::env::var(format!("{prefix}_ALLOW_HTTP")).as_deref() == Ok("true");
    Ok(config)
}

pub async fn run(command: CacheCommand) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let durable_config = config("MTC_DURABLE_S3")?;
    let cache_config = config("MTC_CACHE_S3")?;
    if durable_config.s3_endpoint == cache_config.s3_endpoint
        && durable_config.s3_bucket == cache_config.s3_bucket
    {
        return Err("cache and durable storage must be independent".into());
    }
    let durable = ArchiveStore::from_config(&durable_config).await?;
    let cache = ArchiveStore::from_config(&cache_config).await?;
    match command {
        CacheCommand::Publish {
            tenant,
            digest,
            bytes,
            compressed,
        } => {
            if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err("invalid original digest".into());
            }
            let object = CasArchiveObject {
                object_locator: format!(
                    "tenants/{tenant}/cas/v1/blake3/{}/{digest}{}",
                    &digest[..2],
                    if compressed { ".mtcz1" } else { "" }
                ),
                blake3_digest: digest,
                size_bytes: bytes,
            };
            let key = cache.cache_original(&durable, tenant, &object).await?;
            println!("{}", serde_json::json!({"published":true,"cache_key":key}));
        }
        CacheCommand::Reap {
            high_bytes,
            low_bytes,
            apply,
        } => {
            let result = cache
                .evict_cached_originals(&durable, high_bytes, low_bytes, apply)
                .await?;
            println!("{}", serde_json::to_string(&result)?);
        }
    }
    Ok(())
}
