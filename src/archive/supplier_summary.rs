use bytes::Bytes;
use object_store::{ObjectStoreExt, PutMode, PutOptions, PutPayload};
use uuid::Uuid;

use super::{ArchiveStore, path::archive_path};
use crate::{error::AppError, supplier_error::SupplierError};

fn root(tenant: Uuid, request: Uuid) -> String {
    format!("tenants/{tenant}/raw/v1/{request}/supplier-summary.json")
}

impl ArchiveStore {
    /// This is a fixed-vocabulary projection of the delivered rejection. The
    /// original upstream bytes remain exclusively in the raw capture CAS.
    pub(crate) async fn publish_supplier_summary(
        &self,
        tenant: Uuid,
        request: Uuid,
        envelope: &str,
    ) -> Result<(), AppError> {
        if crate::supplier_error::supplier_error_from_inline_json(Some(envelope)).is_none() {
            return Err(AppError::storage("supplier_summary_invalid"));
        }
        let key = root(tenant, request);
        match self
            .inner
            .put_opts(
                &archive_path(&key)?,
                PutPayload::from_bytes(Bytes::copy_from_slice(envelope.as_bytes())),
                PutOptions {
                    mode: PutMode::Create,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(object_store::Error::AlreadyExists { .. }) => {
                if self.get_bounded(&key, 2048).await?.as_ref() == envelope.as_bytes() {
                    Ok(())
                } else {
                    Err(AppError::Conflict(
                        "supplier summary content conflict".into(),
                    ))
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) async fn supplier_summary(
        &self,
        tenant: Uuid,
        request: Uuid,
    ) -> Result<Option<SupplierError>, AppError> {
        let key = root(tenant, request);
        match self.inner.head(&archive_path(&key)?).await {
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let bytes = self.get_bounded(&key, 2048).await?;
        let envelope = std::str::from_utf8(&bytes)
            .map_err(|_| AppError::storage("supplier_summary_invalid"))?;
        crate::supplier_error::supplier_error_from_inline_json(Some(envelope))
            .map(Some)
            .ok_or_else(|| AppError::storage("supplier_summary_invalid"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn summary_is_immutable_tenant_scoped_and_rejects_private_text() {
        let store = super::super::tests::memory_store();
        let tenant = Uuid::now_v7();
        let request = Uuid::now_v7();
        let safe = r#"inline-json:{"error":{"type":"upstream_error","code":"no_active_plan","message":"当前账号没有可用套餐","mtc_safe_reason":"no_active_plan"}}"#;
        store
            .publish_supplier_summary(tenant, request, safe)
            .await
            .unwrap();
        store
            .publish_supplier_summary(tenant, request, safe)
            .await
            .unwrap();
        assert_eq!(
            store
                .supplier_summary(tenant, request)
                .await
                .unwrap()
                .unwrap()
                .code,
            "no_active_plan"
        );
        assert!(
            store
                .supplier_summary(Uuid::now_v7(), request)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .publish_supplier_summary(
                    tenant,
                    request,
                    &safe.replace("当前账号没有可用套餐", "private prompt")
                )
                .await
                .is_err()
        );
        let other = r#"inline-json:{"error":{"type":"upstream_error","code":"rate_limited","message":"The upstream request rate limit was exceeded","mtc_safe_reason":"rate_limited"}}"#;
        assert!(
            store
                .publish_supplier_summary(tenant, request, other)
                .await
                .is_err()
        );
    }
}
