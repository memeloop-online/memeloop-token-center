//! Durable ownership for disposable carrier objects. Active receipts are not GC
//! permission; only the final carrier owner may publish a release intent.
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload, path::Path};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::{ArchiveStore, path::archive_path};
use crate::{archive_staging::ArchiveStagingKey, error::AppError};

const OWNERS: &str = "carrier-owners/v1";
const RELEASES: &str = "carrier-releases/v1";
const MAX_METADATA_BYTES: usize = 4096;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CarrierObjectOwner {
    version: u8,
    owner_kind: String,
    owner_id: String,
    purpose: String,
    attempt_id: String,
    backend_identity: Option<String>,
    object_locator: String,
    byte_len: u64,
    blake3_digest: String,
}

impl CarrierObjectOwner {
    pub(crate) fn new(
        key: ArchiveStagingKey,
        locator: String,
        bytes: &Bytes,
    ) -> Result<Self, AppError> {
        crate::archive_staging::validate_bound_locator(key, &locator)?;
        let value = Self {
            version: 1,
            owner_kind: key.owner.kind().into(),
            owner_id: key.owner.id().to_string(),
            purpose: key.purpose.as_str().into(),
            attempt_id: key.attempt_id.to_string(),
            backend_identity: key.backend_identity_text(),
            object_locator: locator,
            byte_len: bytes.len() as u64,
            blake3_digest: blake3::hash(bytes).to_hex().to_string(),
        };
        value.key()?;
        Ok(value)
    }

    fn key(&self) -> Result<ArchiveStagingKey, AppError> {
        if self.version != 1
            || self.blake3_digest.len() != 64
            || blake3::Hash::from_hex(&self.blake3_digest).is_err()
        {
            return Err(invalid());
        }
        let key = ArchiveStagingKey::from_database(
            &self.owner_kind,
            &self.owner_id,
            &self.purpose,
            &self.attempt_id,
        )?
        .with_backend_identity(self.backend_identity.as_deref())?;
        let prefix = format!("{}/carrier-", key.canonical_prefix());
        let suffix = self
            .object_locator
            .strip_prefix(&prefix)
            .ok_or_else(invalid)?;
        let id = uuid::Uuid::parse_str(suffix).map_err(|_| invalid())?;
        if id.to_string() != suffix {
            return Err(invalid());
        }
        crate::archive_staging::validate_bound_locator(key, &self.object_locator)?;
        Ok(key)
    }

    fn path(&self, prefix: &str) -> Result<Path, AppError> {
        let encoded = serde_json::to_vec(self).map_err(|_| invalid())?;
        archive_path(&format!(
            "{prefix}/{}.json",
            blake3::hash(&encoded).to_hex()
        ))
    }
}

fn invalid() -> AppError {
    AppError::storage("carrier_ownership_invalid")
}

async fn read_metadata(store: &Arc<dyn ObjectStore>, path: &Path) -> Result<Bytes, AppError> {
    let result = store.get(path).await?;
    if result.meta.size > MAX_METADATA_BYTES as u64 {
        return Err(invalid());
    }
    let mut stream = result.into_stream();
    let mut output = BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if chunk.len() > MAX_METADATA_BYTES.saturating_sub(output.len()) {
            return Err(invalid());
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output.freeze())
}

async fn immutable_metadata(
    store: &Arc<dyn ObjectStore>,
    path: &Path,
    bytes: Bytes,
) -> Result<(), AppError> {
    match store
        .put_opts(
            path,
            PutPayload::from_bytes(bytes.clone()),
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
    if read_metadata(store, path).await? != bytes {
        return Err(invalid());
    }
    Ok(())
}

impl ArchiveStore {
    /// Must be confirmed before starting any body upload. An unknown receipt
    /// write cannot authorize upload or release the sole RAM source.
    pub(crate) async fn begin_carrier_object(
        &self,
        owner: &CarrierObjectOwner,
    ) -> Result<(), AppError> {
        let key = owner.key()?;
        let (store, _) = self.resolve(&key.canonical_prefix())?;
        let bytes = serde_json::to_vec(owner).map_err(|_| invalid())?;
        immutable_metadata(store, &owner.path(OWNERS)?, Bytes::from(bytes)).await
    }

    /// Persist retryable cleanup responsibility before the first deletion.
    /// Called only after the last source/reader/publisher Arc has gone away.
    pub(crate) async fn release_carrier_object(
        &self,
        owner: &CarrierObjectOwner,
    ) -> Result<(), AppError> {
        let key = owner.key()?;
        let (store, _) = self.resolve(&key.canonical_prefix())?;
        immutable_metadata(
            store,
            &owner.path(RELEASES)?,
            Bytes::from(serde_json::to_vec(owner).map_err(|_| invalid())?),
        )
        .await?;
        self.finish_carrier_release(store, owner).await
    }

    async fn finish_carrier_release(
        &self,
        store: &Arc<dyn ObjectStore>,
        owner: &CarrierObjectOwner,
    ) -> Result<(), AppError> {
        let key = owner.key()?;
        let deletion = self
            .delete_archive_staging_object(key, &owner.object_locator)
            .await;
        let (_, object_path) = self.resolve(&owner.object_locator)?;
        match store.head(&object_path).await {
            Err(object_store::Error::NotFound { .. }) => {}
            Ok(_) => {
                return Err(deletion
                    .err()
                    .unwrap_or_else(|| AppError::storage("carrier_cleanup_not_confirmed")));
            }
            Err(error) => return Err(error.into()),
        }
        // A missing owner is valid after an interrupted previous cleanup. The
        // release itself contains the complete immutable identity for recovery.
        match read_metadata(store, &owner.path(OWNERS)?).await {
            Ok(bytes)
                if bytes.as_ref()
                    != serde_json::to_vec(owner).map_err(|_| invalid())?.as_slice() =>
            {
                return Err(invalid());
            }
            Ok(_) => store.delete(&owner.path(OWNERS)?).await.map(|_| ())?,
            Err(AppError::Storage { .. }) => match store.head(&owner.path(OWNERS)?).await {
                Err(object_store::Error::NotFound { .. }) => {}
                _ => return Err(AppError::storage("carrier_owner_cleanup_deferred")),
            },
            Err(error) => return Err(error),
        }
        store.delete(&owner.path(RELEASES)?).await?;
        Ok(())
    }

    /// The existing reaper resumes released objects without depending on RAM
    /// Drop tasks. Active receipts remain protected pending real release proof.
    pub(crate) async fn reap_released_carrier_objects(&self) -> Result<u64, AppError> {
        let prefix = archive_path(RELEASES)?;
        let mut stores = vec![(self.inner.clone(), None)];
        if let Some(training) = &self.training {
            stores.push((training.store.clone(), Some(training.identity.clone())));
        }
        let mut cleaned = 0;
        for (store, identity) in stores {
            let mut listed = store.list(Some(&prefix));
            let mut observed = 0;
            while let Some(item) = listed.next().await {
                if observed >= 4 {
                    break;
                }
                observed += 1;
                let meta = item?;
                let bytes = read_metadata(&store, &meta.location).await?;
                let owner: CarrierObjectOwner =
                    serde_json::from_slice(&bytes).map_err(|_| invalid())?;
                owner.key()?;
                if owner.backend_identity != identity || owner.path(RELEASES)? != meta.location {
                    return Err(invalid());
                }
                match self.finish_carrier_release(&store, &owner).await {
                    Ok(()) => cleaned += 1,
                    Err(_) => tracing::warn!(
                        stage = "carrier_cleanup",
                        "durable carrier cleanup deferred"
                    ),
                }
            }
        }
        Ok(cleaned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive_staging::{ArchiveStagingOwner, ArchiveStagingPurpose};

    #[tokio::test]
    async fn active_owner_is_retained_and_durable_release_resumes_exact_cleanup() {
        let archive = super::super::tests::memory_store();
        let key = ArchiveStagingKey::new(
            ArchiveStagingOwner::ProxyRequest(uuid::Uuid::now_v7()),
            ArchiveStagingPurpose::Request,
            uuid::Uuid::now_v7(),
        )
        .unwrap();
        let locator = format!(
            "{}/carrier-{}",
            key.canonical_prefix(),
            uuid::Uuid::now_v7()
        );
        let bytes = Bytes::from_static(b"only temporary source");
        let owner = CarrierObjectOwner::new(key, locator.clone(), &bytes).unwrap();
        archive.begin_carrier_object(&owner).await.unwrap();
        archive.put(&locator, bytes.clone()).await.unwrap();
        let neighbor = format!(
            "{}/carrier-{}",
            key.canonical_prefix(),
            uuid::Uuid::now_v7()
        );
        archive.put(&neighbor, bytes.clone()).await.unwrap();
        assert_eq!(archive.reap_released_carrier_objects().await.unwrap(), 0);
        assert_eq!(archive.get(&locator).await.unwrap(), bytes);
        // Model a crash after release intent, before body deletion. A fresh
        // pass recovers from remote metadata without the original RAM owner.
        immutable_metadata(
            &archive.inner,
            &owner.path(RELEASES).unwrap(),
            Bytes::from(serde_json::to_vec(&owner).unwrap()),
        )
        .await
        .unwrap();
        assert_eq!(
            archive
                .clone()
                .reap_released_carrier_objects()
                .await
                .unwrap(),
            1
        );
        assert!(
            archive
                .inner
                .head(&archive_path(&locator).unwrap())
                .await
                .is_err()
        );
        assert_eq!(archive.get(&neighbor).await.unwrap(), bytes);
        assert_eq!(archive.reap_released_carrier_objects().await.unwrap(), 0);
    }
}
