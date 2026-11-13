//! Dedicated immutable recovery inbox; never traverses supplier payload archives.
use super::ArchiveStore;
use crate::error::AppError;
use bytes::Bytes;
use futures_util::StreamExt;
use object_store::{PutMode, PutOptions, PutPayload};

pub(crate) const PREFIX: &str = "quota-reset-results/v1/";
impl ArchiveStore {
    pub(crate) async fn publish_quota_reset_receipt(
        &self,
        key: &str,
        bytes: Bytes,
    ) -> Result<(), AppError> {
        let path = object_store::path::Path::from(key);
        match self
            .inner
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
                if self.get_bounded(key, 4096).await? == bytes {
                    Ok(())
                } else {
                    Err(AppError::storage("quota_reset_receipt_conflict"))
                }
            }
            Err(error) => Err(error.into()),
        }
    }
    pub(crate) async fn quota_reset_receipts(
        &self,
        after: Option<&str>,
    ) -> Result<Vec<String>, AppError> {
        let prefix = object_store::path::Path::from(PREFIX);
        let offset = after.map(object_store::path::Path::from);
        let mut objects = match offset.as_ref() {
            Some(offset) => self.inner.list_with_offset(Some(&prefix), offset),
            None => self.inner.list(Some(&prefix)),
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
