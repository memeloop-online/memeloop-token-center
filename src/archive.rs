use std::{ops::Range, path::PathBuf, pin::Pin, sync::Arc, time::Duration};

use bytes::{Bytes, BytesMut};
use futures_util::{Stream, StreamExt, TryStreamExt, stream};
use object_store::{
    GetOptions, GetRange, ObjectStore, ObjectStoreExt, PutPayload, RetryConfig,
    aws::AmazonS3Builder, memory::InMemory, path::Path,
};

use crate::{
    archive_staging::ArchiveStagingKey,
    config::{ArchiveBackend, Config},
    error::AppError,
};

const DEFAULT_ARCHIVE_READ_LIMIT: usize = 16 * 1024 * 1024;
// A gateway may archive many streaming responses concurrently. `WriteMultipart`
// owns every queued part until its backend upload finishes, so parallel 5 MiB
// parts multiply directly by request concurrency. Keep one part in flight per
// object; request-level concurrency still supplies aggregate S3 parallelism
// while each individual writer applies bounded backpressure.
const ARCHIVE_MULTIPART_PART_BYTES: usize = 5 * 1024 * 1024;
const ARCHIVE_MULTIPART_MAX_IN_FLIGHT_PARTS: usize = 1;
const READINESS_SUCCESS_TTL: Duration = Duration::from_secs(5 * 60);
const READINESS_FAILURE_TTL: Duration = Duration::from_secs(10);
const READINESS_DEADLINE: Duration = Duration::from_millis(1_500);
const READINESS_CANARY: &[u8] = b"memeloop-token-center/archive-readiness/v1";

#[derive(Clone)]
pub struct ArchiveStore {
    inner: Arc<dyn ObjectStore>,
    readiness: Arc<tokio::sync::Mutex<ReadinessCache>>,
}

#[derive(Default)]
struct ReadinessCache {
    checked_at: Option<tokio::time::Instant>,
    healthy: bool,
}

#[must_use = "an archive writer must be finished or explicitly aborted"]
pub struct ArchiveWriter {
    store: Arc<dyn ObjectStore>,
    staging: Path,
    inner: Option<object_store::WriteMultipart>,
    multipart_part_bytes: usize,
    hasher: blake3::Hasher,
    size_bytes: u64,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct StagedArchiveObject {
    pub object_locator: String,
    pub blake3_digest: String,
    pub size_bytes: u64,
}

/// A bounded-memory object-store read. The stream keeps filesystem and S3
/// responses incremental instead of collecting a generated image or video in
/// the API process.
pub struct ArchiveDownload {
    pub object_size: u64,
    pub range: Range<u64>,
    pub stream: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>,
}

/// The narrow object-store capability used by the durable staging reaper.
///
/// Both operations accept a typed key rather than a database or caller supplied
/// path. Implementations must derive the canonical prefix from that key and
/// apply segment-boundary matching to every object returned by a lexical list.
#[async_trait::async_trait]
pub trait ArchiveStagingObjectStore: Send + Sync {
    async fn delete_archive_staging_segment(&self, key: ArchiveStagingKey) -> Result<(), AppError>;

    async fn archive_staging_segment_is_empty(
        &self,
        key: ArchiveStagingKey,
    ) -> Result<bool, AppError>;
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

        Ok(Self {
            inner,
            readiness: Arc::new(tokio::sync::Mutex::new(ReadinessCache::default())),
        })
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
        self.delete_segment_prefix(prefix).await
    }

    /// Deletes exactly one typed staging segment, including an object whose key
    /// equals the segment and all segment descendants. A lexical UUID neighbour
    /// can never enter the deletion stream.
    pub async fn delete_archive_staging_segment(
        &self,
        key: ArchiveStagingKey,
    ) -> Result<(), AppError> {
        self.delete_segment_prefix(archive_path(&key.canonical_prefix())?)
            .await
    }

    /// Verifies that the exact typed segment is empty. As with deletion, S3's
    /// lexical list results are filtered again at the path-segment boundary.
    pub async fn archive_staging_segment_is_empty(
        &self,
        key: ArchiveStagingKey,
    ) -> Result<bool, AppError> {
        let prefix = archive_path(&key.canonical_prefix())?;
        match self.inner.head(&prefix).await {
            Ok(_) => return Ok(false),
            Err(object_store::Error::NotFound { .. }) => {}
            Err(error) => return Err(error.into()),
        }

        let mut objects = self.inner.list(Some(&prefix));
        while let Some(metadata) = objects.next().await {
            let metadata = metadata?;
            if metadata.location.prefix_matches(&prefix) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn delete_segment_prefix(&self, prefix: Path) -> Result<(), AppError> {
        // ObjectStore::list is allowed to use a raw lexical prefix (notably on
        // S3), so `staging/x` can also return `staging/x2`. Filter every result
        // with Path's segment-aware matcher before allowing bulk deletion.
        // `list` normally omits an object whose key exactly equals the prefix;
        // HEAD it separately because callers also use this method to clean an
        // exact staged object locator.
        let exact = match self.inner.head(&prefix).await {
            Ok(_) => Some(Ok(prefix.clone())),
            Err(object_store::Error::NotFound { .. }) => None,
            Err(error) => return Err(error.into()),
        };
        let listed_prefix = prefix.clone();
        let descendants = self
            .inner
            .list(Some(&prefix))
            .try_filter(move |metadata| {
                futures_util::future::ready(
                    metadata.location != listed_prefix
                        && metadata.location.prefix_matches(&listed_prefix),
                )
            })
            .map_ok(|metadata| metadata.location)
            .boxed();
        let locations = stream::iter(exact).chain(descendants).boxed();
        self.inner
            .delete_stream(locations)
            .try_for_each(|_| futures_util::future::ready(Ok(())))
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

    pub async fn head_size(&self, location: &str) -> Result<u64, AppError> {
        Ok(self.inner.head(&archive_path(location)?).await?.size)
    }

    pub async fn open_stream(
        &self,
        location: &str,
        range: Option<Range<u64>>,
    ) -> Result<ArchiveDownload, AppError> {
        let path = archive_path(location)?;
        let requested_range = range.clone();
        let result = self
            .inner
            .get_opts(
                &path,
                GetOptions {
                    range: range.map(GetRange::Bounded),
                    ..GetOptions::default()
                },
            )
            .await?;
        let object_size = result.meta.size;
        let returned_range = result.range.clone();
        let expected_bytes =
            validate_download_range(object_size, requested_range.as_ref(), &returned_range)?;
        let stream = verified_download_stream(result.into_stream(), expected_bytes);
        Ok(ArchiveDownload {
            object_size,
            range: returned_range,
            stream,
        })
    }

    pub async fn readiness_check(&self) -> Result<(), AppError> {
        let mut cache = self.readiness.lock().await;
        if let Some(checked_at) = cache.checked_at {
            let ttl = if cache.healthy {
                READINESS_SUCCESS_TTL
            } else {
                READINESS_FAILURE_TTL
            };
            if checked_at.elapsed() < ttl {
                return if cache.healthy {
                    Ok(())
                } else {
                    Err(AppError::Storage(
                        "archive readiness canary failed".to_owned(),
                    ))
                };
            }
        }

        // List alone does not prove the application can archive and retrieve a
        // response. Exercise the exact read/write/delete permissions once at
        // startup and then cache the result so ordinary probes do not generate
        // continual object-store writes.
        let check = tokio::time::timeout(READINESS_DEADLINE, async {
            let mut objects = self.inner.list(None);
            if let Some(first) = objects.next().await {
                first.map_err(|_| {
                    AppError::Storage("archive readiness canary operation failed".to_owned())
                })?;
            }
            let location = format!("readiness/{}.bin", uuid::Uuid::now_v7());
            let path = archive_path(&location)?;
            self.inner
                .put(&path, PutPayload::from_static(READINESS_CANARY))
                .await
                .map_err(|_| {
                    AppError::Storage("archive readiness canary operation failed".to_owned())
                })?;
            let read = self.inner.get(&path).await.map_err(|_| {
                AppError::Storage("archive readiness canary operation failed".to_owned())
            });
            let read = match read {
                Ok(read) => read.bytes().await.map_err(|_| {
                    AppError::Storage("archive readiness canary operation failed".to_owned())
                }),
                Err(error) => Err(error),
            };
            let delete = self.inner.delete(&path).await;
            let read = read?;
            delete.map_err(|_| {
                AppError::Storage("archive readiness canary operation failed".to_owned())
            })?;
            if read.as_ref() != READINESS_CANARY {
                return Err(AppError::Storage(
                    "archive readiness canary content mismatch".to_owned(),
                ));
            }
            Ok(())
        })
        .await
        .map_err(|_| AppError::Storage("archive readiness canary timed out".to_owned()))?;
        cache.checked_at = Some(tokio::time::Instant::now());
        cache.healthy = check.is_ok();
        check.map_err(|_| AppError::Storage("archive readiness canary failed".to_owned()))
    }

    pub async fn start_writer(&self, location: &str) -> Result<ArchiveWriter, AppError> {
        let staging = archive_path(location)?;
        let upload = self.inner.put_multipart(&staging).await?;
        Ok(ArchiveWriter {
            store: self.inner.clone(),
            staging,
            inner: Some(object_store::WriteMultipart::new_with_chunk_size(
                upload,
                ARCHIVE_MULTIPART_PART_BYTES,
            )),
            multipart_part_bytes: ARCHIVE_MULTIPART_PART_BYTES,
            hasher: blake3::Hasher::new(),
            size_bytes: 0,
        })
    }
}

#[async_trait::async_trait]
impl ArchiveStagingObjectStore for ArchiveStore {
    async fn delete_archive_staging_segment(&self, key: ArchiveStagingKey) -> Result<(), AppError> {
        ArchiveStore::delete_archive_staging_segment(self, key).await
    }

    async fn archive_staging_segment_is_empty(
        &self,
        key: ArchiveStagingKey,
    ) -> Result<bool, AppError> {
        ArchiveStore::archive_staging_segment_is_empty(self, key).await
    }
}

impl ArchiveWriter {
    pub async fn write(&mut self, mut bytes: Bytes) -> Result<(), AppError> {
        let next_size = self
            .size_bytes
            .checked_add(u64::try_from(bytes.len()).map_err(|_| AppError::Internal)?)
            .ok_or_else(|| AppError::Storage("archive object size overflow".into()))?;
        if self.inner.is_none() {
            return Err(AppError::Storage("archive writer is already closed".into()));
        }
        while !bytes.is_empty() {
            let capacity = self
                .inner
                .as_mut()
                .expect("archive writer was checked open")
                .wait_for_capacity(ARCHIVE_MULTIPART_MAX_IN_FLIGHT_PARTS)
                .await;
            if let Err(error) = capacity {
                let inner = self.inner.take().expect("archive writer was open");
                if let Err(abort_error) = inner.abort().await {
                    tracing::error!(%abort_error, "failed to abort archive multipart upload after a write error");
                }
                return Err(error.into());
            }
            // `WriteMultipart::put` can enqueue multiple parts synchronously
            // when handed one large Bytes. Bound each call to one part so the
            // capacity wait above remains a hard rather than advisory limit.
            let part = bytes.split_to(bytes.len().min(self.multipart_part_bytes));
            self.hasher.update(&part);
            self.size_bytes = self
                .size_bytes
                .checked_add(u64::try_from(part.len()).map_err(|_| AppError::Internal)?)
                .ok_or_else(|| AppError::Storage("archive object size overflow".into()))?;
            self.inner
                .as_mut()
                .expect("archive writer was checked open")
                .put(part);
            let uploaded = self
                .inner
                .as_mut()
                .expect("archive writer was checked open")
                .wait_for_capacity(0)
                .await;
            if let Err(error) = uploaded {
                let inner = self.inner.take().expect("archive writer was open");
                if let Err(abort_error) = inner.abort().await {
                    tracing::error!(%abort_error, "failed to abort archive multipart upload after a write error");
                }
                return Err(error.into());
            }
        }
        debug_assert_eq!(self.size_bytes, next_size);
        Ok(())
    }

    pub async fn finish(mut self) -> Result<String, AppError> {
        self.finish_multipart().await?;
        let location = content_location(
            std::mem::replace(&mut self.hasher, blake3::Hasher::new())
                .finalize()
                .to_hex()
                .as_str(),
        );
        let destination = archive_path(&location)?;
        self.store.copy(&self.staging, &destination).await?;
        self.store.delete(&self.staging).await?;
        Ok(location)
    }

    /// Completes a durable request/job-scoped object without copying it into
    /// the shared content-addressed namespace. The caller may atomically bind
    /// this unique locator in relational metadata, or safely delete its exact
    /// staging prefix if the surrounding operation fails.
    pub async fn finish_staged(mut self) -> Result<StagedArchiveObject, AppError> {
        self.finish_multipart().await?;
        Ok(StagedArchiveObject {
            object_locator: self.staging.to_string(),
            blake3_digest: std::mem::replace(&mut self.hasher, blake3::Hasher::new())
                .finalize()
                .to_hex()
                .to_string(),
            size_bytes: self.size_bytes,
        })
    }

    pub async fn abort(mut self) -> Result<(), AppError> {
        let Some(inner) = self.inner.take() else {
            return Ok(());
        };
        inner.abort().await?;
        Ok(())
    }

    async fn finish_multipart(&mut self) -> Result<(), AppError> {
        let Some(mut inner) = self.inner.take() else {
            return Err(AppError::Storage("archive writer is already closed".into()));
        };

        // object_store::WriteMultipart aborts when `complete` fails, but a
        // failed in-flight part is returned by wait_for_capacity before that
        // branch. Await it while we still own the writer so that failure can
        // also be explicitly aborted instead of being left to bucket cleanup.
        if let Err(error) = inner.wait_for_capacity(0).await {
            if let Err(abort_error) = inner.abort().await {
                tracing::error!(%abort_error, "failed to abort archive multipart upload after a part failure");
            }
            return Err(error.into());
        }
        inner.finish().await?;
        Ok(())
    }
}

impl Drop for ArchiveWriter {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                // Drop cannot await network I/O. Detach a best-effort abort on
                // the current runtime; an S3 lifecycle rule remains mandatory
                // for process crashes and runtime shutdown.
                drop(runtime.spawn(async move {
                    if let Err(error) = inner.abort().await {
                        tracing::error!(%error, "failed to abort dropped archive multipart upload");
                    }
                }));
            }
            Err(_) => tracing::error!(
                "archive multipart writer dropped without a runtime; object-store lifecycle cleanup is required"
            ),
        }
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

fn validate_download_range(
    object_size: u64,
    requested: Option<&Range<u64>>,
    returned: &Range<u64>,
) -> Result<u64, AppError> {
    let expected = requested.cloned().unwrap_or(0..object_size);
    if returned.start > returned.end
        || returned.end > object_size
        || expected.start > expected.end
        || expected.end > object_size
        || *returned != expected
    {
        return Err(AppError::Storage(
            "archive download range mismatch".to_owned(),
        ));
    }
    Ok(expected.end - expected.start)
}

fn verified_download_stream(
    source: Pin<Box<dyn Stream<Item = object_store::Result<Bytes>> + Send>>,
    expected_bytes: u64,
) -> Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> {
    let verified = stream::unfold(
        (source, expected_bytes, false),
        |(mut source, mut remaining, done)| async move {
            if done {
                return None;
            }

            if remaining == 0 {
                loop {
                    match source.next().await {
                        Some(Ok(chunk)) if chunk.is_empty() => continue,
                        None => return None,
                        Some(Ok(_)) => {
                            return Some((
                                Err(std::io::Error::other("archive download length mismatch")),
                                (source, 0, true),
                            ));
                        }
                        Some(Err(_)) => {
                            return Some((
                                Err(std::io::Error::other("archive download stream failed")),
                                (source, 0, true),
                            ));
                        }
                    }
                }
            }

            loop {
                match source.next().await {
                    Some(Err(_)) => {
                        return Some((
                            Err(std::io::Error::other("archive download stream failed")),
                            (source, remaining, true),
                        ));
                    }
                    Some(Ok(chunk)) if chunk.is_empty() => continue,
                    Some(Ok(chunk)) => {
                        let chunk_len = match u64::try_from(chunk.len()) {
                            Ok(length) => length,
                            Err(_) => {
                                return Some((
                                    Err(std::io::Error::other("archive download length mismatch")),
                                    (source, remaining, true),
                                ));
                            }
                        };
                        if chunk_len > remaining {
                            return Some((
                                Err(std::io::Error::other("archive download length mismatch")),
                                (source, remaining, true),
                            ));
                        }
                        remaining -= chunk_len;
                        if remaining == 0 {
                            // Read one item ahead before releasing the final
                            // chunk. This catches a backend that advertises the
                            // right range but returns additional bytes.
                            loop {
                                match source.next().await {
                                    Some(Ok(extra)) if extra.is_empty() => continue,
                                    None => {
                                        return Some((Ok(chunk), (source, 0, true)));
                                    }
                                    Some(Ok(_)) => {
                                        return Some((
                                            Err(std::io::Error::other(
                                                "archive download length mismatch",
                                            )),
                                            (source, 0, true),
                                        ));
                                    }
                                    Some(Err(_)) => {
                                        return Some((
                                            Err(std::io::Error::other(
                                                "archive download stream failed",
                                            )),
                                            (source, 0, true),
                                        ));
                                    }
                                }
                            }
                        }
                        return Some((Ok(chunk), (source, remaining, false)));
                    }
                    None => {
                        return Some((
                            Err(std::io::Error::new(
                                std::io::ErrorKind::UnexpectedEof,
                                "archive download ended before the advertised range",
                            )),
                            (source, remaining, true),
                        ));
                    }
                }
            }
        },
    );
    Box::pin(verified)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures_util::FutureExt;
    use object_store::{Extensions, MultipartUpload, PutResult, UploadPart};

    use super::*;

    #[derive(Debug)]
    struct TestMultipartUpload {
        aborts: Arc<AtomicUsize>,
        fail_parts: bool,
    }

    #[async_trait::async_trait]
    impl MultipartUpload for TestMultipartUpload {
        fn put_part(&mut self, _data: PutPayload) -> UploadPart {
            let fail = self.fail_parts;
            async move {
                if fail {
                    Err(object_store::Error::Generic {
                        store: "archive-test",
                        source: Box::new(std::io::Error::other("injected part failure")),
                    })
                } else {
                    Ok(())
                }
            }
            .boxed()
        }

        async fn complete(&mut self) -> object_store::Result<PutResult> {
            Ok(PutResult {
                e_tag: None,
                version: None,
                extensions: Extensions::new(),
            })
        }

        async fn abort(&mut self) -> object_store::Result<()> {
            self.aborts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn memory_store() -> ArchiveStore {
        ArchiveStore {
            inner: Arc::new(InMemory::new()),
            readiness: Arc::new(tokio::sync::Mutex::new(ReadinessCache::default())),
        }
    }

    fn test_writer(aborts: Arc<AtomicUsize>, fail_parts: bool) -> ArchiveWriter {
        let upload = TestMultipartUpload { aborts, fail_parts };
        ArchiveWriter {
            store: Arc::new(InMemory::new()),
            staging: Path::from("staging/test/response.bin"),
            inner: Some(object_store::WriteMultipart::new_with_chunk_size(
                Box::new(upload),
                1,
            )),
            multipart_part_bytes: 1,
            hasher: blake3::Hasher::new(),
            size_bytes: 0,
        }
    }

    fn test_download_stream(
        chunks: Vec<object_store::Result<Bytes>>,
        expected_bytes: u64,
    ) -> Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> {
        verified_download_stream(Box::pin(stream::iter(chunks)), expected_bytes)
    }

    #[tokio::test]
    async fn repeated_content_reuses_one_stable_object_location() {
        let store = memory_store();
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
        let store = memory_store();
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
    async fn archive_store_allows_twelve_writers_to_make_progress() {
        let store = memory_store();
        let mut writers = Vec::new();
        for index in 0..12 {
            writers.push(
                tokio::time::timeout(
                    Duration::from_secs(1),
                    store.start_writer(&format!("staging/writer-limit/{index}")),
                )
                .await
                .expect("creating a writer must not wait on another writer")
                .expect("writer"),
            );
        }
        for writer in &mut writers {
            tokio::time::timeout(
                Duration::from_secs(1),
                writer.write(Bytes::from_static(b"x")),
            )
            .await
            .expect("all twelve writers must make progress independently")
            .expect("writer progress");
        }
        for writer in writers {
            writer.abort().await.expect("cleanup writer");
        }
    }

    #[tokio::test]
    async fn staged_streams_are_durable_isolated_and_exactly_cleanable() {
        let store = memory_store();
        let mut first = store
            .start_writer("staging/synchronous/request-a/asset-0")
            .await
            .expect("first staged writer");
        first
            .write(Bytes::from_static(b"first image"))
            .await
            .expect("first staged write");
        let first = first.finish_staged().await.expect("first staged finish");
        let mut second = store
            .start_writer("staging/synchronous/request-b/asset-0")
            .await
            .expect("second staged writer");
        second
            .write(Bytes::from_static(b"second image"))
            .await
            .expect("second staged write");
        let second = second.finish_staged().await.expect("second staged finish");

        assert_eq!(first.size_bytes, b"first image".len() as u64);
        assert_eq!(
            first.blake3_digest,
            blake3::hash(b"first image").to_hex().to_string()
        );
        assert_eq!(
            store
                .get(&first.object_locator)
                .await
                .expect("first object"),
            Bytes::from_static(b"first image")
        );
        assert_eq!(
            store
                .get(&second.object_locator)
                .await
                .expect("second object"),
            Bytes::from_static(b"second image")
        );
        assert!(
            store
                .head_size(&content_location(&first.blake3_digest))
                .await
                .is_err(),
            "finish_staged must not promote into the shared CAS namespace"
        );

        store
            .delete_prefix("staging/synchronous/request-a")
            .await
            .expect("delete exact first staging prefix");
        assert!(store.get(&first.object_locator).await.is_err());
        assert_eq!(
            store
                .get(&second.object_locator)
                .await
                .expect("isolated second object"),
            Bytes::from_static(b"second image")
        );
    }

    #[tokio::test]
    async fn delete_prefix_is_segment_exact_and_rejects_non_canonical_input() {
        let temporary = tempfile::tempdir().expect("temporary archive root");
        let mut filesystem_config = Config::for_test("sqlite::memory:".to_owned());
        filesystem_config.archive_backend = ArchiveBackend::Filesystem;
        filesystem_config.archive_path = Some(temporary.path().to_string_lossy().into_owned());
        let filesystem = ArchiveStore::from_config(&filesystem_config)
            .await
            .expect("filesystem archive store");

        for store in [memory_store(), filesystem] {
            store
                .put("staging/exact-object", Bytes::from_static(b"exact"))
                .await
                .expect("put exact-prefix object");
            store
                .put("staging/x/object", Bytes::from_static(b"x"))
                .await
                .expect("put target descendant");
            store
                .put("staging/x2/object", Bytes::from_static(b"x2"))
                .await
                .expect("put lexical neighbour");

            store
                .delete_prefix("staging/exact-object")
                .await
                .expect("delete object exactly equal to prefix");
            assert!(store.get("staging/exact-object").await.is_err());
            store
                .delete_prefix("staging/x")
                .await
                .expect("delete exact path segment");
            assert!(store.get("staging/x/object").await.is_err());
            assert_eq!(
                store
                    .get("staging/x2/object")
                    .await
                    .expect("lexical neighbour survives"),
                Bytes::from_static(b"x2")
            );

            for unsafe_prefix in [
                "",
                "/",
                ".",
                "..",
                "staging/./x",
                "staging/../x",
                "staging//x",
                "staging/x/",
                "staging\\x",
                "staging/%2e%2e/x",
            ] {
                assert!(
                    matches!(
                        store.delete_prefix(unsafe_prefix).await,
                        Err(AppError::BadRequest(_))
                    ),
                    "unsafe prefix {unsafe_prefix:?} must fail locally"
                );
            }
        }
    }

    #[tokio::test]
    async fn bounded_read_accepts_an_object_exactly_at_the_limit() {
        let store = memory_store();
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
        let store = memory_store();
        let body = Bytes::from_static(b"one-byte-too-large");
        let location = store.put_content(body.clone()).await.expect("put content");

        let error = store
            .get_bounded(&location, body.len() - 1)
            .await
            .expect_err("object over limit must be rejected");
        assert!(error.to_string().contains("read limit"));
    }

    #[tokio::test]
    async fn stream_read_returns_the_exact_requested_range_without_collecting_in_store() {
        let store = memory_store();
        let location = store
            .put_content(Bytes::from_static(b"0123456789"))
            .await
            .expect("put ranged fixture");
        let download = store
            .open_stream(&location, Some(2..7))
            .await
            .expect("open bounded stream");
        assert_eq!(download.object_size, 10);
        assert_eq!(download.range, 2..7);
        let chunks = download
            .stream
            .try_collect::<Vec<_>>()
            .await
            .expect("read bounded stream");
        assert_eq!(chunks.concat(), b"23456");
    }

    #[tokio::test]
    async fn download_metadata_rejects_wrong_or_out_of_bounds_ranges() {
        assert_eq!(
            validate_download_range(10, Some(&(2..7)), &(2..7)).expect("exact range"),
            5
        );
        assert!(validate_download_range(10, Some(&(2..7)), &(2..8)).is_err());
        assert!(validate_download_range(10, Some(&(2..7)), &(1..7)).is_err());
        assert!(validate_download_range(10, Some(&(2..11)), &(2..11)).is_err());
        assert!(validate_download_range(10, None, &(0..9)).is_err());
    }

    #[tokio::test]
    async fn download_stream_fails_closed_on_short_or_extra_bodies() {
        let short = test_download_stream(vec![Ok(Bytes::from_static(b"12"))], 3)
            .try_collect::<Vec<_>>()
            .await
            .expect_err("short response must fail");
        assert_eq!(short.kind(), std::io::ErrorKind::UnexpectedEof);

        let oversized = test_download_stream(vec![Ok(Bytes::from_static(b"1234"))], 3)
            .try_collect::<Vec<_>>()
            .await
            .expect_err("oversized chunk must fail before release");
        assert_eq!(oversized.kind(), std::io::ErrorKind::Other);

        let trailing = test_download_stream(
            vec![
                Ok(Bytes::from_static(b"12")),
                Ok(Bytes::from_static(b"3")),
                Ok(Bytes::from_static(b"4")),
            ],
            3,
        )
        .try_collect::<Vec<_>>()
        .await
        .expect_err("trailing bytes must fail before the final chunk is released");
        assert_eq!(trailing.kind(), std::io::ErrorKind::Other);

        let exact = test_download_stream(
            vec![
                Ok(Bytes::from_static(b"12")),
                Ok(Bytes::new()),
                Ok(Bytes::from_static(b"3")),
            ],
            3,
        )
        .try_collect::<Vec<_>>()
        .await
        .expect("exact response");
        assert_eq!(exact.concat(), b"123");

        let empty = test_download_stream(Vec::new(), 0)
            .try_collect::<Vec<_>>()
            .await
            .expect("empty object response");
        assert!(empty.is_empty());
        test_download_stream(vec![Ok(Bytes::from_static(b"unexpected"))], 0)
            .try_collect::<Vec<_>>()
            .await
            .expect_err("a zero-length range cannot return bytes");
    }

    #[tokio::test]
    async fn multipart_part_failure_is_aborted_before_write_returns() {
        let aborts = Arc::new(AtomicUsize::new(0));
        let mut writer = test_writer(aborts.clone(), true);
        writer
            .write(Bytes::from_static(b"p"))
            .await
            .expect_err("part failure must fail the write that queued it");
        assert_eq!(aborts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn multipart_large_write_applies_backpressure_between_parts() {
        let aborts = Arc::new(AtomicUsize::new(0));
        let mut writer = test_writer(aborts.clone(), true);
        writer
            .write(Bytes::from_static(b"part"))
            .await
            .expect_err("one input buffer must be backpressured between multipart chunks");
        assert_eq!(aborts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dropped_multipart_writer_schedules_best_effort_abort() {
        let aborts = Arc::new(AtomicUsize::new(0));
        let mut writer = test_writer(aborts.clone(), false);
        writer
            .write(Bytes::from_static(b"part"))
            .await
            .expect("queue part");
        drop(writer);

        tokio::time::timeout(Duration::from_secs(1), async {
            while aborts.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("drop abort task completes");
        assert_eq!(aborts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn readiness_canary_is_cleaned_and_success_is_cached() {
        let store = memory_store();
        store.readiness_check().await.expect("first canary");
        store.readiness_check().await.expect("cached canary");
        let prefix = Path::from("readiness");
        let remaining = store
            .inner
            .list(Some(&prefix))
            .try_collect::<Vec<_>>()
            .await
            .expect("list readiness prefix");
        assert!(remaining.is_empty(), "readiness canary must be deleted");
    }

    #[tokio::test]
    async fn object_locations_reject_traversal_and_ambiguous_separators() {
        let store = memory_store();

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
                "tenants/test-tenant/objects/request_01.bin",
                Bytes::from_static(b"safe"),
            )
            .await
            .expect("safe tenant object location");
    }
}
