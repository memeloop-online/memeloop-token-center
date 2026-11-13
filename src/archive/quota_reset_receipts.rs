//! Dedicated immutable recovery inbox; never traverses supplier payload archives.
use super::ArchiveStore;
use crate::error::AppError;
use bytes::Bytes;
use futures_util::StreamExt;
use object_store::{PutMode, PutOptions, PutPayload};

pub(crate) const PREFIX: &str = "quota-reset-results/v1/";
impl ArchiveStore {
    // Receipt durability is explicitly the configured RustFS training backend,
    // independent of legacy archive/CAS rollout flags. A backend switch must
    // drain the old identity with its old configuration before retiring it.
    pub(crate) fn quota_reset_backend_identity(&self) -> String {
        self.training
            .as_ref()
            .map(|store| store.identity.clone())
            .unwrap_or_default()
    }
    fn quota_reset_store(
        &self,
        identity: &str,
    ) -> Result<&std::sync::Arc<dyn object_store::ObjectStore>, AppError> {
        let store = self
            .training
            .as_ref()
            .ok_or_else(|| AppError::storage("quota_reset_receipt_backend_missing"))?;
        if identity != store.identity {
            return Err(AppError::storage("quota_reset_receipt_backend_mismatch"));
        }
        Ok(&store.store)
    }
    pub(crate) async fn publish_quota_reset_receipt(
        &self,
        identity: &str,
        key: &str,
        bytes: Bytes,
    ) -> Result<(), AppError> {
        let store = self.quota_reset_store(identity)?;
        let path = object_store::path::Path::from(key);
        match store
            .put_opts(
                &path,
                PutPayload::from_bytes(bytes.clone()),
                PutOptions {
                    mode: PutMode::Create,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(object_store::Error::AlreadyExists { .. }) => {
                if self.read_quota_reset_receipt(identity, key).await? == bytes {
                    Ok(())
                } else {
                    Err(AppError::storage("quota_reset_receipt_conflict"))
                }
            }
            Err(error) => Err(error.into()),
        }
    }
    pub(crate) async fn read_quota_reset_receipt(
        &self,
        identity: &str,
        key: &str,
    ) -> Result<Bytes, AppError> {
        use object_store::ObjectStoreExt;
        let store = self.quota_reset_store(identity)?;
        let path = object_store::path::Path::from(key);
        if store.head(&path).await?.size > 4096 {
            return Err(AppError::storage("quota_reset_receipt_too_large"));
        }
        let mut stream = store.get(&path).await?.into_stream();
        let mut bytes = bytes::BytesMut::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len() + chunk.len() > 4096 {
                return Err(AppError::storage("quota_reset_receipt_too_large"));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes.freeze())
    }
    pub(crate) async fn delete_quota_reset_receipt(
        &self,
        identity: &str,
        key: &str,
    ) -> Result<(), AppError> {
        self.quota_reset_store(identity)?
            .delete(&object_store::path::Path::from(key))
            .await?;
        Ok(())
    }
    pub(crate) async fn quota_reset_receipts(
        &self,
        after: Option<&str>,
    ) -> Result<Vec<String>, AppError> {
        let identity = self.quota_reset_backend_identity();
        let store = self.quota_reset_store(&identity)?;
        let prefix = object_store::path::Path::from(format!("{PREFIX}{identity}/"));
        let offset = after.map(object_store::path::Path::from);
        let mut objects = match offset.as_ref() {
            Some(offset) => store.list_with_offset(Some(&prefix), offset),
            None => store.list(Some(&prefix)),
        };
        let mut keys = Vec::new();
        while let Some(object) = objects.next().await {
            keys.push(object?.location.to_string());
            if keys.len() >= 100 {
                break;
            }
        }
        Ok(keys)
    }
}
