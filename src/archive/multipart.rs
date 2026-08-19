use std::sync::Arc;

use bytes::Bytes;
use object_store::{ObjectStore, ObjectStoreExt, path::Path};

use super::{ArchiveStore, path::archive_path, path::content_location};
use crate::error::AppError;

// A gateway may archive many streaming responses concurrently. `WriteMultipart`
// owns every queued part until its backend upload finishes, so parallel 5 MiB
// parts multiply directly by request concurrency. Keep one part in flight per
// object; request-level concurrency still supplies aggregate S3 parallelism
// while each individual writer applies bounded backpressure.
const ARCHIVE_MULTIPART_PART_BYTES: usize = 5 * 1024 * 1024;
const ARCHIVE_MULTIPART_MAX_IN_FLIGHT_PARTS: usize = 1;

#[must_use = "an archive writer must be finished or explicitly aborted"]
pub struct ArchiveWriter {
    pub(super) store: Arc<dyn ObjectStore>,
    pub(super) staging: Path,
    pub(super) inner: Option<object_store::WriteMultipart>,
    pub(super) multipart_part_bytes: usize,
    pub(super) hasher: blake3::Hasher,
    pub(super) size_bytes: u64,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct StagedArchiveObject {
    pub object_locator: String,
    pub blake3_digest: String,
    pub size_bytes: u64,
}

impl ArchiveStore {
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
                if inner.abort().await.is_err() {
                    tracing::error!("failed to abort archive multipart upload after a write error");
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
                if inner.abort().await.is_err() {
                    tracing::error!("failed to abort archive multipart upload after a write error");
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
            if inner.abort().await.is_err() {
                tracing::error!("failed to abort archive multipart upload after a part failure");
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
                    if inner.abort().await.is_err() {
                        tracing::error!("failed to abort dropped archive multipart upload");
                    }
                }));
            }
            Err(_) => tracing::error!(
                "archive multipart writer dropped without a runtime; object-store lifecycle cleanup is required"
            ),
        }
    }
}
