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
    store: Arc<dyn ObjectStore>,
    staging: Path,
    inner: object_store::WriteMultipart,
    hasher: blake3::Hasher,
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

    pub async fn put_content(&self, data: Bytes) -> Result<String, AppError> {
        let location = content_location(blake3::hash(&data).to_hex().as_str());
        self.put(&location, data).await?;
        Ok(location)
    }

    pub async fn get(&self, location: &str) -> Result<Bytes, AppError> {
        Ok(self.inner.get(&Path::from(location)).await?.bytes().await?)
    }

    pub async fn start_writer(&self, location: &str) -> Result<ArchiveWriter, AppError> {
        let staging = Path::from(location);
        let upload = self.inner.put_multipart(&staging).await?;
        Ok(ArchiveWriter {
            store: self.inner.clone(),
            staging,
            inner: object_store::WriteMultipart::new(upload),
            hasher: blake3::Hasher::new(),
        })
    }
}

impl ArchiveWriter {
    pub async fn write(&mut self, bytes: Bytes) -> Result<(), AppError> {
        self.hasher.update(&bytes);
        self.inner.wait_for_capacity(4).await?;
        self.inner.put(bytes);
        Ok(())
    }

    pub async fn finish(self) -> Result<String, AppError> {
        let Self {
            store,
            staging,
            inner,
            hasher,
        } = self;
        inner.finish().await?;
        let location = content_location(hasher.finalize().to_hex().as_str());
        let destination = Path::from(location.as_str());
        store.copy(&staging, &destination).await?;
        store.delete(&staging).await?;
        Ok(location)
    }

    pub async fn abort(self) -> Result<(), AppError> {
        self.inner.abort().await?;
        Ok(())
    }
}

fn content_location(hash: &str) -> String {
    format!("objects/blake3/{}/{hash}", &hash[..2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn repeated_content_reuses_one_stable_object_location() {
        let store = ArchiveStore {
            inner: Arc::new(InMemory::new()),
        };
        let body = Bytes::from_static(b"same archived body");

        let first = store.put_content(body.clone()).await.expect("first put");
        let second = store.put_content(body.clone()).await.expect("second put");

        assert_eq!(first, second);
        assert!(first.starts_with("objects/blake3/"));
        assert_eq!(store.get(&first).await.expect("stored content"), body);
    }

    #[tokio::test]
    async fn streamed_content_moves_from_staging_to_its_digest_location() {
        let store = ArchiveStore {
            inner: Arc::new(InMemory::new()),
        };
        let staging = "staging/request-id/response.bin";
        let mut writer = store.start_writer(staging).await.expect("writer");
        writer
            .write(Bytes::from_static(b"stream "))
            .await
            .expect("first chunk");
        writer
            .write(Bytes::from_static(b"body"))
            .await
            .expect("second chunk");

        let location = writer.finish().await.expect("finish");

        assert!(location.starts_with("objects/blake3/"));
        assert_eq!(
            store.get(&location).await.expect("stored stream"),
            Bytes::from_static(b"stream body")
        );
        assert!(store.inner.head(&Path::from(staging)).await.is_err());
    }
}
