use std::{path::PathBuf, sync::Arc, time::Duration};

use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, TryStreamExt};
use object_store::{
    ObjectStore, ObjectStoreExt, PutPayload, RetryConfig, aws::AmazonS3Builder, memory::InMemory,
    path::Path,
};

use crate::{
    config::{ArchiveBackend, Config},
    error::AppError,
};

const DEFAULT_ARCHIVE_READ_LIMIT: usize = 16 * 1024 * 1024;

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
                        .with_allow_http(config.s3_allow_http)
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
        let location = archive_path(location)?;
        self.inner
            .put(&location, PutPayload::from_bytes(data))
            .await?;
        Ok(())
    }

    pub async fn put_content(&self, data: Bytes) -> Result<String, AppError> {
        let location = content_location(blake3::hash(&data).to_hex().as_str());
        self.put(&location, data).await?;
        Ok(location)
    }

    pub async fn delete(&self, location: &str) -> Result<(), AppError> {
        self.inner.delete(&archive_path(location)?).await?;
        Ok(())
    }

    pub async fn delete_prefix(&self, prefix: &str) -> Result<(), AppError> {
        let prefix = archive_path(prefix)?;
        let locations = self
            .inner
            .list(Some(&prefix))
            .map_ok(|metadata| metadata.location)
            .boxed();
        self.inner
            .delete_stream(locations)
            .try_collect::<Vec<Path>>()
            .await?;
        Ok(())
    }

    pub async fn get_bounded(&self, location: &str, maximum: usize) -> Result<Bytes, AppError> {
        let path = archive_path(location)?;
        let metadata = self.inner.head(&path).await?;
        if metadata.size > maximum as u64 {
            return Err(AppError::Storage(format!(
                "archive object exceeds {maximum} byte read limit"
            )));
        }

        let initial_capacity = usize::try_from(metadata.size)
            .map_err(|_| AppError::Storage("archive object size exceeds this platform".into()))?;
        let mut body = BytesMut::with_capacity(initial_capacity);
        let mut stream = self.inner.get(&path).await?.into_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if body.len().saturating_add(chunk.len()) > maximum {
                return Err(AppError::Storage(format!(
                    "archive object exceeds {maximum} byte read limit"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body.freeze())
    }

    pub async fn get(&self, location: &str) -> Result<Bytes, AppError> {
        self.get_bounded(location, DEFAULT_ARCHIVE_READ_LIMIT).await
    }

    pub async fn readiness_check(&self) -> Result<(), AppError> {
        // A listing is non-mutating but still verifies endpoint reachability,
        // authentication and bucket access for both S3 and test backends.
        // Keep this below the chart's one-second probe deadline. Dropping the timed-out
        // future also prevents failed S3 probes from accumulating retrying requests.
        tokio::time::timeout(Duration::from_millis(750), async {
            let mut objects = self.inner.list(None);
            if let Some(first) = objects.next().await {
                first?;
            }
            Ok(())
        })
        .await
        .map_err(|_| AppError::Storage("archive readiness check timed out".to_owned()))?
    }

    pub async fn start_writer(&self, location: &str) -> Result<ArchiveWriter, AppError> {
        let staging = archive_path(location)?;
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
        let destination = archive_path(&location)?;
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

fn archive_path(location: &str) -> Result<Path, AppError> {
    // Object locations are internal identifiers, not filesystem paths or URLs. Keeping
    // their alphabet deliberately small gives every backend (especially the local test
    // backend) the same traversal and separator semantics.
    let has_only_safe_bytes = location
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'));
    if location.is_empty()
        || location.starts_with('/')
        || location.ends_with('/')
        || !has_only_safe_bytes
    {
        return Err(AppError::BadRequest(
            "invalid archive object location".to_owned(),
        ));
    }

    Path::parse(location)
        .map_err(|_| AppError::BadRequest("invalid archive object location".to_owned()))
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
        assert_eq!(
            store
                .get_bounded(&first, body.len())
                .await
                .expect("stored content"),
            body
        );
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
            store
                .get_bounded(&location, b"stream body".len())
                .await
                .expect("stored stream"),
            Bytes::from_static(b"stream body")
        );
        assert!(store.inner.head(&Path::from(staging)).await.is_err());
    }

    #[tokio::test]
    async fn bounded_read_accepts_an_object_exactly_at_the_limit() {
        let store = ArchiveStore {
            inner: Arc::new(InMemory::new()),
        };
        let body = Bytes::from_static(b"exactly-at-limit");
        let location = store.put_content(body.clone()).await.expect("put content");

        assert_eq!(
            store
                .get_bounded(&location, body.len())
                .await
                .expect("read at exact limit"),
            body
        );
    }

    #[tokio::test]
    async fn bounded_read_rejects_an_object_over_the_limit() {
        let store = ArchiveStore {
            inner: Arc::new(InMemory::new()),
        };
        let body = Bytes::from_static(b"one-byte-too-large");
        let location = store.put_content(body.clone()).await.expect("put content");

        let error = store
            .get_bounded(&location, body.len() - 1)
            .await
            .expect_err("object over limit must be rejected");
        assert!(error.to_string().contains("read limit"));
    }

    #[tokio::test]
    async fn object_locations_reject_traversal_and_ambiguous_separators() {
        let store = ArchiveStore {
            inner: Arc::new(InMemory::new()),
        };

        for location in [
            "../escape",
            "tenant/../escape",
            "tenant/./object",
            "/absolute/object",
            "tenant//object",
            "tenant/object/",
            "tenant\\object",
            "tenant/%2e%2e/object",
        ] {
            let error = store
                .put(location, Bytes::from_static(b"must not be stored"))
                .await
                .expect_err("unsafe location must be rejected");
            assert!(matches!(error, AppError::BadRequest(_)), "{location}");
            assert!(!error.to_string().contains(location));
        }

        store
            .put(
                "tenants/tenant-019f/objects/request_01.bin",
                Bytes::from_static(b"safe"),
            )
            .await
            .expect("safe tenant object location");
    }
}
