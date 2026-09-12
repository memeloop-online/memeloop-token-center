use std::{path::PathBuf, sync::Arc, time::Duration};

use object_store::{
    ClientOptions, ObjectStore, RetryConfig, aws::AmazonS3Builder, memory::InMemory,
};

use super::{ArchiveStore, ReadinessCache, path::archive_path};
use crate::{
    config::{ArchiveBackend, Config},
    error::AppError,
};

impl ArchiveStore {
    pub async fn from_config(config: &Config) -> Result<Self, AppError> {
        config
            .s3_timeouts
            .validate()
            .map_err(|_| AppError::BadRequest("invalid S3 timeout configuration".into()))?;
        let inner: Arc<dyn ObjectStore> =
            match config.archive_backend {
                ArchiveBackend::Memory => Arc::new(InMemory::new()),
                ArchiveBackend::Filesystem => {
                    let root = config.archive_path.as_ref().ok_or_else(|| {
                        AppError::BadRequest("MTC_ARCHIVE_PATH is required".into())
                    })?;
                    std::fs::create_dir_all(root).map_err(|_| {
                        AppError::Storage("archive filesystem initialization failed".into())
                    })?;
                    Arc::new(
                        object_store::local::LocalFileSystem::new_with_prefix(PathBuf::from(root))
                            .map_err(|_| {
                                AppError::Storage(
                                    "archive filesystem configuration is invalid".into(),
                                )
                            })?,
                    )
                }
                ArchiveBackend::S3 => {
                    let mut builder = AmazonS3Builder::new()
                        .with_bucket_name(config.s3_bucket.as_ref().ok_or_else(|| {
                            AppError::BadRequest("MTC_S3_BUCKET is required".into())
                        })?)
                        .with_region(&config.s3_region)
                        .with_allow_http(config.s3_allow_http)
                        .with_client_options(
                            ClientOptions::new()
                                .with_allow_http(config.s3_allow_http)
                                .with_connect_timeout(Duration::from_millis(
                                    config.s3_timeouts.connect_millis.into(),
                                ))
                                .with_timeout(Duration::from_millis(
                                    config.s3_timeouts.request_millis.into(),
                                )),
                        )
                        .with_retry(RetryConfig {
                            max_retries: 3,
                            retry_timeout: Duration::from_secs(10),
                            ..RetryConfig::default()
                        });
                    if let Some(endpoint) = &config.s3_endpoint {
                        builder = builder.with_endpoint(endpoint);
                    }
                    if let Some(access_key) = &config.s3_access_key {
                        builder = builder.with_access_key_id(access_key);
                    }
                    if let Some(secret_key) = &config.s3_secret_key {
                        builder = builder.with_secret_access_key(secret_key);
                    }
                    Arc::new(builder.build().map_err(|_| {
                        AppError::Storage("archive S3 configuration is invalid".into())
                    })?)
                }
            };

        let readiness_path = archive_path(&format!("readiness/{}.bin", uuid::Uuid::now_v7()))?;
        Ok(Self {
            inner,
            readiness: Arc::new(tokio::sync::Mutex::new(ReadinessCache::default())),
            readiness_path,
            readiness_deadline: Duration::from_millis(config.s3_timeouts.readiness_millis.into()),
            readiness_metrics: Arc::default(),
        })
    }
}
