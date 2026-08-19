use std::time::Duration;

use futures_util::StreamExt;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

use super::{ArchiveStore, path::archive_path};
use crate::error::AppError;

const READINESS_SUCCESS_TTL: Duration = Duration::from_secs(5 * 60);
const READINESS_FAILURE_TTL: Duration = Duration::from_secs(10);
const READINESS_DEADLINE: Duration = Duration::from_millis(1_500);
const READINESS_CANARY: &[u8] = b"memeloop-token-center/archive-readiness/v1";

impl ArchiveStore {
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
}
