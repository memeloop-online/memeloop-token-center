use std::{path::PathBuf, sync::Arc};

use bytes::Bytes;
use object_store::{
    ObjectStore, ObjectStoreExt, PutPayload, aws::AmazonS3Builder, memory::InMemory, path::Path,
};

use crate::{
    config::{ArchiveBackend, Config},
    error::AppError,
};

#[derive(Clone)]
pub struct ArchiveStore {
    inner: Arc<dyn ObjectStore>,
}

pub struct ArchiveWriter {
    inner: object_store::WriteMultipart,
}

impl ArchiveStore {
    pub async fn from_config(config: &Config) -> Result<Self, AppError> {
        let inner: Arc<dyn ObjectStore> =
            match config.archive_backend {
                ArchiveBackend::Memory => Arc::new(InMemory::new()),
                ArchiveBackend::Filesystem => {
                    let root = config.archive_path.as_ref().ok_or_else(|| {
                        AppError::BadRequest("MTC_ARCHIVE_PATH is required".into())
                    })?;
                    std::fs::create_dir_all(root)
                        .map_err(|error| AppError::Storage(error.to_string()))?;
                    Arc::new(
                        object_store::local::LocalFileSystem::new_with_prefix(PathBuf::from(root))
                            .map_err(|error| AppError::Storage(error.to_string()))?,
                    )
                }
                ArchiveBackend::S3 => {
                    let mut builder = AmazonS3Builder::new()
                        .with_bucket_name(config.s3_bucket.as_ref().ok_or_else(|| {
                            AppError::BadRequest("MTC_S3_BUCKET is required".into())
                        })?)
                        .with_region(&config.s3_region)
                        .with_allow_http(config.s3_allow_http);
                    if let Some(endpoint) = &config.s3_endpoint {
                        builder = builder.with_endpoint(endpoint);
                    }
                    if let Some(access_key) = &config.s3_access_key {
                        builder = builder.with_access_key_id(access_key);
                    }
                    if let Some(secret_key) = &config.s3_secret_key {
                        builder = builder.with_secret_access_key(secret_key);
                    }
                    Arc::new(
                        builder
                            .build()
                            .map_err(|error| AppError::Storage(error.to_string()))?,
                    )
                }
            };

        Ok(Self { inner })
    }

    pub async fn put(&self, location: &str, data: Bytes) -> Result<(), AppError> {
        self.inner
            .put(&Path::from(location), PutPayload::from_bytes(data))
            .await?;
        Ok(())
    }

    pub async fn get(&self, location: &str) -> Result<Bytes, AppError> {
        Ok(self.inner.get(&Path::from(location)).await?.bytes().await?)
    }

    pub async fn start_writer(&self, location: &str) -> Result<ArchiveWriter, AppError> {
        let upload = self.inner.put_multipart(&Path::from(location)).await?;
        Ok(ArchiveWriter {
            inner: object_store::WriteMultipart::new(upload),
        })
    }
}

impl ArchiveWriter {
    pub async fn write(&mut self, bytes: Bytes) -> Result<(), AppError> {
        self.inner.wait_for_capacity(4).await?;
        self.inner.put(bytes);
        Ok(())
    }

    pub async fn finish(self) -> Result<(), AppError> {
        self.inner.finish().await?;
        Ok(())
    }

    pub async fn abort(self) -> Result<(), AppError> {
        self.inner.abort().await?;
        Ok(())
    }
}
