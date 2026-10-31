use bytes::Bytes;
use futures_util::StreamExt;
use object_store::{ObjectStoreExt, PutMode, PutOptions, PutPayload};
use uuid::Uuid;

use super::{ArchiveStore, StagedArchiveObject, path};
use crate::error::AppError;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CasArchiveObject {
    pub object_locator: String,
    pub blake3_digest: String,
    pub size_bytes: u64,
}

impl CasArchiveObject {
    pub(super) fn validate_tenant(&self, tenant_id: Uuid) -> Result<(), AppError> {
        let compressed = self.object_locator.ends_with(super::compressed::SUFFIX);
        if path::tenant_cas_location(tenant_id, &self.blake3_digest, compressed)?
            != self.object_locator
        {
            return Err(AppError::storage("archive_cas_reference_invalid"));
        }
        Ok(())
    }
}

impl ArchiveStore {
    pub async fn put_tenant_content(
        &self,
        tenant_id: Uuid,
        data: Bytes,
    ) -> Result<CasArchiveObject, AppError> {
        let blake3_digest = blake3::hash(&data).to_hex().to_string();
        let size_bytes = data.len() as u64;
        let object_locator = path::tenant_cas_location(tenant_id, &blake3_digest, false)?;
        match self
            .inner
            .put_opts(
                &path::archive_path(&object_locator)?,
                PutPayload::from_bytes(data),
                PutOptions {
                    mode: PutMode::Create,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) | Err(object_store::Error::AlreadyExists { .. }) => {}
            Err(error) => return Err(error.into()),
        }
        self.verify_exact_object(&object_locator, size_bytes, &blake3_digest)
            .await?;
        Ok(CasArchiveObject {
            object_locator,
            blake3_digest,
            size_bytes,
        })
    }

    pub async fn get_tenant_content(
        &self,
        tenant_id: Uuid,
        object: &CasArchiveObject,
        maximum: usize,
    ) -> Result<Bytes, AppError> {
        object.validate_tenant(tenant_id)?;
        if object.size_bytes > maximum as u64 {
            return Err(AppError::storage("archive_cas_reference_invalid"));
        }
        let data = self.get_bounded(&object.object_locator, maximum).await?;
        if data.len() as u64 != object.size_bytes
            || blake3::hash(&data).to_hex().as_str() != object.blake3_digest
        {
            return Err(AppError::storage("archive_cas_object_integrity_failure"));
        }
        Ok(data)
    }

    pub async fn verify_tenant_content(
        &self,
        tenant_id: Uuid,
        object: &CasArchiveObject,
    ) -> Result<(), AppError> {
        object.validate_tenant(tenant_id)?;
        self.verify_exact_object(
            &object.object_locator,
            object.size_bytes,
            &object.blake3_digest,
        )
        .await
    }

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

        self.verify_exact_object(
            &staged.object_locator,
            staged.size_bytes,
            &staged.blake3_digest,
        )
        .await?;
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
            return Err(AppError::storage("local_storage_failed"));
        }
        let mut stream = download.stream;
        let mut size = 0_u64;
        let mut digest = blake3::Hasher::new();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|_| AppError::storage("archive_cas_object_integrity_failure"))?;
            size = size
                .checked_add(
                    u64::try_from(chunk.len())
                        .map_err(|_| AppError::internal("cas_verify_exact_object_1"))?,
                )
                .ok_or_else(|| AppError::storage("archive_cas_object_integrity_failure"))?;
            digest.update(&chunk);
        }
        if size != expected_size || digest.finalize().to_hex().as_str() != expected_digest {
            return Err(AppError::storage("local_storage_failed"));
        }
        Ok(())
    }
}
