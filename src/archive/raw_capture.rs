use bytes::Bytes;
use object_store::{ObjectStoreExt, PutMode, PutOptions, PutPayload};
use uuid::Uuid;

use super::{ArchiveStore, CasArchiveObject, path::archive_path};
use crate::error::AppError;

fn root(tenant: Uuid, request: Uuid, side: &str) -> Result<String, AppError> {
    if !matches!(side, "request" | "response") {
        return Err(AppError::BadRequest("invalid raw archive side".into()));
    }
    Ok(format!("tenants/{tenant}/raw/v1/{request}/{side}.json"))
}

impl ArchiveStore {
    pub async fn publish_raw_capture_failure(
        &self,
        tenant: Uuid,
        request: Uuid,
        side: &str,
    ) -> Result<(), AppError> {
        let key = format!("{}.failed", root(tenant, request, side)?);
        match self
            .inner
            .put_opts(
                &archive_path(&key)?,
                PutPayload::from_bytes(Bytes::from_static(
                    br#"{"format_version":1,"reason":"capture_failed"}"#,
                )),
                PutOptions {
                    mode: PutMode::Create,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) | Err(object_store::Error::AlreadyExists { .. }) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn raw_capture_failed(
        &self,
        tenant: Uuid,
        request: Uuid,
        side: &str,
    ) -> Result<bool, AppError> {
        let key = format!("{}.failed", root(tenant, request, side)?);
        match self.inner.head(&archive_path(&key)?).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn publish_raw_capture(
        &self,
        tenant: Uuid,
        request: Uuid,
        side: &str,
        object: &CasArchiveObject,
    ) -> Result<(), AppError> {
        self.verify_tenant_content(tenant, object).await?;
        let key = root(tenant, request, side)?;
        let encoded = serde_json::to_vec(object)
            .map_err(|_| AppError::internal("raw_capture_reference_encode"))?;
        match self
            .inner
            .put_opts(
                &archive_path(&key)?,
                PutPayload::from_bytes(Bytes::from(encoded)),
                PutOptions {
                    mode: PutMode::Create,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(object_store::Error::AlreadyExists { .. }) => {
                if self
                    .raw_capture_reference(tenant, request, side)
                    .await?
                    .as_ref()
                    == Some(object)
                {
                    Ok(())
                } else {
                    Err(AppError::Conflict(
                        "raw archive root content conflict".into(),
                    ))
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    pub async fn raw_capture_reference(
        &self,
        tenant: Uuid,
        request: Uuid,
        side: &str,
    ) -> Result<Option<CasArchiveObject>, AppError> {
        let key = root(tenant, request, side)?;
        match self.inner.head(&archive_path(&key)?).await {
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let encoded = self.get_bounded(&key, 4096).await?;
        let reference: CasArchiveObject = serde_json::from_slice(&encoded)
            .map_err(|_| AppError::storage("raw_capture_reference_invalid"))?;
        reference.validate_tenant(tenant)?;
        Ok(Some(reference))
    }
}
