//! Disposable duplicate originals. Durable CAS is never a deletion target.

use std::{cmp::Reverse, collections::BinaryHeap};

use futures_util::StreamExt;
use object_store::{ObjectStore, path::Path};
use uuid::Uuid;

use super::{ArchiveStore, CasArchiveObject};
use crate::error::AppError;

const PREFIX: &str = "raw-cache/v1/";
const CANDIDATES: usize = 128;

#[derive(Default, serde::Serialize)]
pub struct CacheFifoPass {
    pub observed_bytes: u64,
    pub eligible_bytes: u64,
    pub deleted_bytes: u64,
    pub deleted_objects: u64,
    pub remaining_observed_bytes: u64,
    pub protected_objects: u64,
    pub dry_run: bool,
}

fn cached_reference(key: &str) -> Option<(Uuid, CasArchiveObject)> {
    let parts: Vec<_> = key.strip_prefix(PREFIX)?.split('/').collect();
    if parts.len() != 5 || Uuid::parse_str(parts[0]).ok()?.get_version_num() != 7 {
        return None;
    }
    let tenant = Uuid::parse_str(parts[1]).ok()?;
    let digest = parts[2];
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let size = parts[3].parse::<u64>().ok()?;
    let suffix = match parts[4] {
        "plain" => "",
        "mtcz1" => ".mtcz1",
        _ => return None,
    };
    Some((
        tenant,
        CasArchiveObject {
            object_locator: format!(
                "tenants/{tenant}/cas/v1/blake3/{}/{digest}{suffix}",
                &digest[..2]
            ),
            blake3_digest: digest.into(),
            size_bytes: size,
        },
    ))
}

impl ArchiveStore {
    /// Publish a disposable duplicate only after streaming the durable original
    /// through its hash verifier. Multipart uploads are not listed as complete
    /// cache entries, so the FIFO cannot select an active publication.
    pub async fn cache_original(
        &self,
        durable: &ArchiveStore,
        tenant: Uuid,
        original: &CasArchiveObject,
    ) -> Result<String, AppError> {
        let mut source = durable.open_tenant_content(tenant, original).await?.stream;
        let format = if original.object_locator.ends_with(".mtcz1") {
            "mtcz1"
        } else {
            "plain"
        };
        let key = format!(
            "{PREFIX}{}/{tenant}/{}/{}/{format}",
            Uuid::now_v7(),
            original.blake3_digest,
            original.size_bytes
        );
        let mut writer = self.start_writer(&key).await?;
        while let Some(chunk) = source.next().await {
            match chunk {
                Ok(chunk) => writer.write(chunk).await?,
                Err(_) => {
                    writer.abort().await?;
                    return Err(AppError::storage("cache_source_integrity_failure"));
                }
            }
        }
        let completed = writer.finish_staged().await?;
        if completed.size_bytes != original.size_bytes
            || completed.blake3_digest != original.blake3_digest
        {
            return Err(AppError::storage("cache_source_integrity_failure"));
        }
        Ok(key)
    }

    /// One bounded FIFO deletion batch. Only this module's completed duplicate
    /// entries are eligible, and their durable originals are reverified before
    /// deletion. All other cache-prefix objects remain protected.
    pub async fn evict_cached_originals(
        &self,
        durable: &ArchiveStore,
        high_bytes: u64,
        low_bytes: u64,
        apply: bool,
    ) -> Result<CacheFifoPass, AppError> {
        if high_bytes == 0 || low_bytes >= high_bytes {
            return Err(AppError::BadRequest("invalid cache watermarks".into()));
        }
        let mut pass = CacheFifoPass {
            dry_run: !apply,
            ..Default::default()
        };
        // Keep only the oldest bounded candidate batch; do not collect a
        // potentially multi-million-object bucket listing in memory.
        let mut oldest = BinaryHeap::new();
        let prefix = Path::from(PREFIX);
        let mut objects = self.inner.list(Some(&prefix));
        while let Some(metadata) = objects.next().await {
            let metadata = metadata?;
            let key = metadata.location.to_string();
            if !key.starts_with(PREFIX) {
                continue;
            }
            pass.observed_bytes = pass.observed_bytes.saturating_add(metadata.size);
            if cached_reference(&key).is_none() {
                pass.protected_objects += 1;
                continue;
            }
            oldest.push((
                metadata.last_modified,
                key,
                metadata.size,
                metadata.e_tag,
                metadata.version,
            ));
            if oldest.len() > CANDIDATES {
                oldest.pop();
            }
        }
        pass.remaining_observed_bytes = pass.observed_bytes;
        if pass.observed_bytes <= high_bytes {
            return Ok(pass);
        }
        let mut ordered = BinaryHeap::from(
            oldest
                .into_vec()
                .into_iter()
                .map(Reverse)
                .collect::<Vec<_>>(),
        );
        let mut projected = pass.observed_bytes;
        while projected > low_bytes {
            let Some(Reverse((_, key, size, etag, version))) = ordered.pop() else {
                break;
            };
            let (tenant, original) = cached_reference(&key)
                .ok_or_else(|| AppError::storage("invalid_cache_candidate"))?;
            if size != original.size_bytes
                || durable
                    .verify_tenant_content(tenant, &original)
                    .await
                    .is_err()
            {
                pass.protected_objects += 1;
                continue;
            }
            let path = Path::from(key);
            let current = match self.inner.head(&path).await {
                Ok(current) => current,
                Err(object_store::Error::NotFound { .. }) => continue,
                Err(error) => return Err(error.into()),
            };
            if current.size != size || current.e_tag != etag || current.version != version {
                pass.protected_objects += 1;
                continue;
            }
            pass.eligible_bytes = pass.eligible_bytes.saturating_add(size);
            projected = projected.saturating_sub(size);
            if apply {
                self.inner.delete(&path).await?;
                pass.deleted_bytes = pass.deleted_bytes.saturating_add(size);
                pass.deleted_objects += 1;
                pass.remaining_observed_bytes = pass.remaining_observed_bytes.saturating_sub(size);
            }
        }
        Ok(pass)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::archive::tests::memory_store;

    #[tokio::test]
    async fn fifo_reclaims_only_verified_duplicates_and_preserves_durable_originals() {
        let durable = memory_store();
        let cache = memory_store();
        let tenant = Uuid::now_v7();
        let original = durable
            .put_tenant_content(tenant, Bytes::from_static(b"original"))
            .await
            .unwrap();
        let first = cache
            .cache_original(&durable, tenant, &original)
            .await
            .unwrap();
        let second = cache
            .cache_original(&durable, tenant, &original)
            .await
            .unwrap();
        cache
            .put("raw-cache/v1/unowned", Bytes::from_static(b"x"))
            .await
            .unwrap();
        let preview = cache
            .evict_cached_originals(&durable, 16, 9, false)
            .await
            .unwrap();
        assert_eq!(preview.observed_bytes, 17);
        assert_eq!(preview.eligible_bytes, 8);
        assert_eq!(preview.deleted_bytes, 0);
        assert!(cache.head_size(&first).await.is_ok());
        let applied = cache
            .evict_cached_originals(&durable, 16, 9, true)
            .await
            .unwrap();
        assert_eq!(applied.deleted_objects, 1);
        assert_eq!(applied.remaining_observed_bytes, 9);
        assert!(cache.head_size(&first).await.is_err());
        assert_eq!(
            cache.get(&second).await.unwrap(),
            Bytes::from_static(b"original")
        );
        assert_eq!(
            cache.get("raw-cache/v1/unowned").await.unwrap(),
            Bytes::from_static(b"x")
        );
        durable
            .verify_tenant_content(tenant, &original)
            .await
            .unwrap();

        // A missing durable original makes even our own cache entry ineligible.
        let missing = memory_store();
        let protected = cache
            .evict_cached_originals(&missing, 8, 1, true)
            .await
            .unwrap();
        assert_eq!(protected.deleted_objects, 0);
        assert_eq!(protected.protected_objects, 2);
        assert!(cache.head_size(&second).await.is_ok());
    }
}
