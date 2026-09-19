use futures_util::StreamExt;
use object_store::ObjectStoreExt;
use uuid::Uuid;

use super::{ArchiveStore, StagedArchiveObject, path};
use crate::error::AppError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CasArchiveObject {
    pub object_locator: String,
    pub blake3_digest: String,
    pub size_bytes: u64,
}

impl ArchiveStore {
    /// Publishes one completed text staging object into tenant-local immutable
    /// CAS. The source remains in staging until the relational locator and the
    /// exact staging cleanup transition commit together.
    pub async fn promote_staged_text_to_cas(
        &self,
        tenant_id: Uuid,
        staged: &StagedArchiveObject,
    ) -> Result<CasArchiveObject, AppError> {
        let compressed = staged.object_locator.ends_with(super::compressed::SUFFIX);
        let locator = path::tenant_cas_location(tenant_id, &staged.blake3_digest, compressed)?;
        let source = path::archive_path(&staged.object_locator)?;
        let destination = path::archive_path(&locator)?;

        match self.inner.copy_if_not_exists(&source, &destination).await {
            Ok(()) | Err(object_store::Error::AlreadyExists { .. }) => {}
            Err(error) => return Err(error.into()),
        }

        self.verify_exact_object(&locator, staged.size_bytes, &staged.blake3_digest)
            .await?;
        Ok(CasArchiveObject {
            object_locator: locator,
            blake3_digest: staged.blake3_digest.clone(),
            size_bytes: staged.size_bytes,
        })
    }

    async fn verify_exact_object(
        &self,
        locator: &str,
        expected_size: u64,
        expected_digest: &str,
    ) -> Result<(), AppError> {
        let download = self.open_stream(locator, None).await?;
        if download.object_size != expected_size {
            return Err(AppError::Storage(
                "archive CAS object integrity failure".into(),
            ));
        }
        let mut stream = download.stream;
        let mut size = 0_u64;
        let mut digest = blake3::Hasher::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk
                .map_err(|_| AppError::Storage("archive CAS object integrity failure".into()))?;
            size = size
                .checked_add(u64::try_from(chunk.len()).map_err(|_| AppError::Internal)?)
                .ok_or_else(|| AppError::Storage("archive CAS object integrity failure".into()))?;
            digest.update(&chunk);
        }
        if size != expected_size || digest.finalize().to_hex().as_str() != expected_digest {
            return Err(AppError::Storage(
                "archive CAS object integrity failure".into(),
            ));
        }
        Ok(())
    }
}
