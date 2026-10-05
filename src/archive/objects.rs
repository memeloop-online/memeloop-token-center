use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use object_store::{ObjectStoreExt, PutPayload};

use super::{
    ArchiveStore,
    path::{archive_path, content_location, is_any_v1_cas_location},
};
use crate::error::AppError;

const DEFAULT_ARCHIVE_READ_LIMIT: usize = 16 * 1024 * 1024;

impl ArchiveStore {
    pub async fn put(&self, location: &str, data: Bytes) -> Result<(), AppError> {
        if is_any_v1_cas_location(location) {
            return Err(AppError::BadRequest(
                "archive CAS objects are immutable".into(),
            ));
        }
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
        if is_any_v1_cas_location(location) {
            return Err(AppError::BadRequest(
                "archive CAS objects are immutable".into(),
            ));
        }
        self.inner.delete(&archive_path(location)?).await?;
        Ok(())
    }

    pub async fn get_bounded(&self, location: &str, maximum: usize) -> Result<Bytes, AppError> {
        if location.ends_with(super::compressed::SUFFIX) {
            let download = self.open_stream(location, None).await?;
            if download.object_size > maximum as u64 {
                return Err(AppError::Storage(
                    "archive object exceeds read limit".into(),
                ));
            }
            let mut body = BytesMut::with_capacity(download.object_size as usize);
            let mut stream = download.stream;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk
                    .map_err(|_| AppError::Storage("archive compressed read failed".into()))?;
                if body.len().saturating_add(chunk.len()) > maximum {
                    return Err(AppError::Storage(
                        "archive object exceeds read limit".into(),
                    ));
                }
                body.extend_from_slice(&chunk);
            }
            return Ok(body.freeze());
        }
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
        if location.ends_with(super::compressed::SUFFIX) {
            return self.compressed_size(location).await;
        }
        Ok(self.inner.head(&archive_path(location)?).await?.size)
    }
}
