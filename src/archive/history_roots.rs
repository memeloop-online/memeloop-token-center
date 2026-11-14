use bytes::Bytes;
use object_store::{PutMode, PutOptions, PutPayload};
use uuid::Uuid;

use super::{ArchiveStore, CasArchiveObject, path::archive_path};
use crate::error::AppError;

/// An immutable owner of an already verified CAS object. Publishing or ACKing
/// a ClickHouse row never removes this root. URI provenance is not ownership.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryArchiveRoot {
    pub version: u8,
    pub tenant_id: Uuid,
    pub key_id: Uuid,
    pub source: String,
    pub request_id: Uuid,
    pub created_at: i64,
    pub revision: i64,
    pub object: CasArchiveObject,
}

impl HistoryArchiveRoot {
    pub fn location(&self) -> Result<String, AppError> {
        if self.version != 1
            || self.revision <= 0
            || !matches!(
                self.source.as_str(),
                "native" | "generation" | "archive" | "event"
            )
        {
            return Err(AppError::storage("history_archive_root_invalid"));
        }
        self.object.validate_tenant(self.tenant_id)?;
        Ok(format!(
            "tenants/{}/history/v1/{}/{}/{}/{}.json",
            self.tenant_id, self.source, self.request_id, self.revision, self.object.blake3_digest
        ))
    }

    pub(super) fn decode(location: &str, bytes: &[u8]) -> Result<Self, AppError> {
        let root: Self = serde_json::from_slice(bytes)
            .map_err(|_| AppError::storage("history_archive_root_invalid"))?;
        if root.location()? != location {
            return Err(AppError::storage("history_archive_root_identity_mismatch"));
        }
        Ok(root)
    }
}

impl ArchiveStore {
    /// Verify the complete typed object before publishing its independent root.
    /// Create/unknown/retry cannot overwrite a different owner at this identity.
    pub async fn publish_history_root(
        &self,
        root: &HistoryArchiveRoot,
    ) -> Result<String, AppError> {
        let location = root.location()?;
        self.verify_tenant_content(root.tenant_id, &root.object)
            .await?;
        let bytes = Bytes::from(
            serde_json::to_vec(root)
                .map_err(|_| AppError::storage("history_archive_root_invalid"))?,
        );
        let path = archive_path(&location)?;
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
            Ok(_) => {}
            Err(object_store::Error::AlreadyExists { .. }) => {
                if self.get_bounded(&location, bytes.len()).await? != bytes {
                    return Err(AppError::Conflict(
                        "history archive root content conflict".into(),
                    ));
                }
            }
            Err(error) => return Err(error.into()),
        }
        // A successful or ambiguous retry must prove the durable owner too.
        if Self::read_history_root_bytes(
            &location,
            &self.get_bounded(&location, bytes.len()).await?,
        )? != *root
        {
            return Err(AppError::storage("history_archive_root_identity_mismatch"));
        }
        Ok(location)
    }

    pub fn read_history_root_bytes(
        location: &str,
        bytes: &[u8],
    ) -> Result<HistoryArchiveRoot, AppError> {
        HistoryArchiveRoot::decode(location, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::TryStreamExt;

    #[tokio::test]
    async fn request_history_roots_reuse_cas_and_keep_exact_owner_after_retry() {
        let store = super::super::tests::memory_store();
        let tenant = Uuid::now_v7();
        let body = Bytes::from_static(b"history originals without a local body file");
        let object = store
            .put_tenant_content(tenant, body.clone())
            .await
            .unwrap();
        assert_eq!(
            store
                .put_tenant_content(tenant, body.clone())
                .await
                .unwrap(),
            object
        );
        let root = HistoryArchiveRoot {
            version: 1,
            tenant_id: tenant,
            key_id: Uuid::now_v7(),
            source: "native".into(),
            request_id: Uuid::now_v7(),
            created_at: 1,
            revision: 2,
            object,
        };
        let location = store.publish_history_root(&root).await.unwrap();
        assert_eq!(store.publish_history_root(&root).await.unwrap(), location);
        let roots: Vec<_> = store.durable_roots().try_collect().await.unwrap();
        assert_eq!(roots, vec![location.clone()]);
        let encoded = store.get(&location).await.unwrap();
        assert_eq!(
            ArchiveStore::read_history_root_bytes(&location, &encoded).unwrap(),
            root
        );
        let mut wrong = root.clone();
        wrong.key_id = Uuid::now_v7();
        assert!(store.publish_history_root(&wrong).await.is_err());
        wrong.tenant_id = Uuid::now_v7();
        assert!(store.publish_history_root(&wrong).await.is_err());
        assert!(store.put(&location, Bytes::new()).await.is_err());
        assert!(store.delete(&location).await.is_err());
        assert!(
            store
                .delete_prefix(&format!("tenants/{tenant}/history"))
                .await
                .is_err()
        );
        assert!(store.start_writer(&location).await.is_err());
        let target = super::super::tests::memory_store();
        assert!(
            target
                .restore_durable_root(&location, encoded.clone())
                .await
                .is_err()
        );
        target.put_tenant_content(tenant, body).await.unwrap();
        target
            .restore_durable_root(&location, encoded.clone())
            .await
            .unwrap();
        target
            .restore_durable_root(&location, encoded)
            .await
            .unwrap();
        assert!(
            target
                .restore_durable_root(&location, Bytes::from_static(b"{}"))
                .await
                .is_err()
        );
        assert_eq!(
            target
                .get_tenant_content(tenant, &root.object, root.object.size_bytes as usize)
                .await
                .unwrap(),
            store
                .get_tenant_content(tenant, &root.object, root.object.size_bytes as usize)
                .await
                .unwrap()
        );
    }
}
