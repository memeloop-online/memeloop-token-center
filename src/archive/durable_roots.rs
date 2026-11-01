use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use object_store::{ObjectStoreExt, PutMode, PutOptions, PutPayload, path::Path};

use super::{
    ArchiveStore,
    path::{archive_path, is_conversation_root_location},
};
use crate::error::AppError;

impl ArchiveStore {
    /// Stream durable root identities, independently of optional SQL indexes.
    /// CAS bodies, staging objects and evictable request caches are excluded.
    /// Consumers must not assume backend listing order or retain a list cursor.
    pub fn durable_roots(&self) -> impl Stream<Item = Result<String, AppError>> + Send + '_ {
        self.inner
            .list(Some(&Path::from("tenants")))
            .filter_map(|entry| async move {
                match entry {
                    Ok(entry) if is_conversation_root_location(entry.location.as_ref()) => {
                        Some(Ok(entry.location.to_string()))
                    }
                    Ok(_) => None,
                    Err(error) => Some(Err(error.into())),
                }
            })
    }

    /// Restore an independently verified durable root without replacing any
    /// existing root. References must be restored and verified by the caller
    /// before publication, just as for a newly captured conversation.
    pub async fn restore_durable_root(&self, location: &str, bytes: Bytes) -> Result<(), AppError> {
        let path = archive_path(location)?;
        if !is_conversation_root_location(location) {
            return Err(AppError::BadRequest("invalid durable root location".into()));
        }
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
                if self.get_bounded(location, bytes.len()).await? == bytes {
                    Ok(())
                } else {
                    Err(AppError::Conflict("durable root content conflict".into()))
                }
            }
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::TryStreamExt;
    use uuid::Uuid;

    #[tokio::test]
    async fn durable_roots_are_discoverable_without_sql_and_cannot_be_replaced() {
        let source = super::super::tests::memory_store();
        let target = super::super::tests::memory_store();
        let tenant = Uuid::now_v7();
        let request = Uuid::now_v7();
        let body = source
            .put_tenant_content(tenant, Bytes::from_static(b"original"))
            .await
            .unwrap();
        source
            .publish_raw_capture(tenant, request, "request", &body)
            .await
            .unwrap();
        source
            .put("staging/proxy/unindexed/body", Bytes::from_static(b"cache"))
            .await
            .unwrap();
        let roots: Vec<_> = source.durable_roots().try_collect().await.unwrap();
        assert_eq!(
            roots,
            vec![format!("tenants/{tenant}/raw/v1/{request}/request.json")]
        );
        let encoded = source.get(&roots[0]).await.unwrap();
        target
            .restore_durable_root(&roots[0], encoded.clone())
            .await
            .unwrap();
        target
            .restore_durable_root(&roots[0], encoded.clone())
            .await
            .unwrap();
        assert!(
            target
                .restore_durable_root(&roots[0], Bytes::from_static(b"changed"))
                .await
                .is_err()
        );
        assert_eq!(target.get(&roots[0]).await.unwrap(), encoded);
        assert!(
            target
                .restore_durable_root(&body.object_locator, Bytes::new())
                .await
                .is_err()
        );
        assert!(target.delete(&roots[0]).await.is_err());
    }
}
