use futures_util::{StreamExt, TryStreamExt, stream};
use object_store::{ObjectStore, ObjectStoreExt, path::Path};

use super::{ArchiveStore, path::archive_path};
use crate::{archive_staging::ArchiveStagingKey, error::AppError};

/// The narrow object-store capability used by the durable staging reaper.
///
/// Both operations accept a typed key rather than a database or caller supplied
/// path. Implementations must derive the canonical prefix from that key and
/// apply segment-boundary matching to every object returned by a lexical list.
#[async_trait::async_trait]
pub trait ArchiveStagingObjectStore: Send + Sync {
    async fn delete_archive_staging_segment(&self, key: ArchiveStagingKey) -> Result<(), AppError>;

    async fn archive_staging_segment_is_empty(
        &self,
        key: ArchiveStagingKey,
    ) -> Result<bool, AppError>;
}

impl ArchiveStore {
    pub async fn delete_prefix(&self, prefix: &str) -> Result<(), AppError> {
        let prefix = archive_path(prefix)?;
        self.delete_segment_prefix(prefix).await
    }

    /// Deletes exactly one typed staging segment, including an object whose key
    /// equals the segment and all segment descendants. A lexical UUID neighbour
    /// can never enter the deletion stream.
    pub async fn delete_archive_staging_segment(
        &self,
        key: ArchiveStagingKey,
    ) -> Result<(), AppError> {
        self.delete_segment_prefix(archive_path(&key.canonical_prefix())?)
            .await
    }

    /// Verifies that the exact typed segment is empty. As with deletion, S3's
    /// lexical list results are filtered again at the path-segment boundary.
    pub async fn archive_staging_segment_is_empty(
        &self,
        key: ArchiveStagingKey,
    ) -> Result<bool, AppError> {
        let prefix = archive_path(&key.canonical_prefix())?;
        match self.inner.head(&prefix).await {
            Ok(_) => return Ok(false),
            Err(object_store::Error::NotFound { .. }) => {}
            Err(error) => return Err(error.into()),
        }

        let mut objects = self.inner.list(Some(&prefix));
        while let Some(metadata) = objects.next().await {
            let metadata = metadata?;
            if metadata.location.prefix_matches(&prefix) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn delete_segment_prefix(&self, prefix: Path) -> Result<(), AppError> {
        // ObjectStore::list is allowed to use a raw lexical prefix (notably on
        // S3), so `staging/x` can also return `staging/x2`. Filter every result
        // with Path's segment-aware matcher before allowing bulk deletion.
        // `list` normally omits an object whose key exactly equals the prefix;
        // HEAD it separately because callers also use this method to clean an
        // exact staged object locator.
        let exact = match self.inner.head(&prefix).await {
            Ok(_) => Some(Ok(prefix.clone())),
            Err(object_store::Error::NotFound { .. }) => None,
            Err(error) => return Err(error.into()),
        };
        let listed_prefix = prefix.clone();
        let descendants = self
            .inner
            .list(Some(&prefix))
            .try_filter(move |metadata| {
                futures_util::future::ready(
                    metadata.location != listed_prefix
                        && metadata.location.prefix_matches(&listed_prefix),
                )
            })
            .map_ok(|metadata| metadata.location)
            .boxed();
        let locations = stream::iter(exact).chain(descendants).boxed();
        self.inner
            .delete_stream(locations)
            .try_for_each(|_| futures_util::future::ready(Ok(())))
            .await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl ArchiveStagingObjectStore for ArchiveStore {
    async fn delete_archive_staging_segment(&self, key: ArchiveStagingKey) -> Result<(), AppError> {
        ArchiveStore::delete_archive_staging_segment(self, key).await
    }

    async fn archive_staging_segment_is_empty(
        &self,
        key: ArchiveStagingKey,
    ) -> Result<bool, AppError> {
        ArchiveStore::archive_staging_segment_is_empty(self, key).await
    }
}
