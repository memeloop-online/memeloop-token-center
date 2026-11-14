//! Request-scoped random-access scratch pages under the existing typed staging
//! lease and janitor. No page directory or ownership journal is kept in RAM.
use super::ArchiveStore;
use crate::{archive_staging::*, db::Database, error::AppError};
use bytes::Bytes;
use futures_util::StreamExt;
use object_store::{ObjectStoreExt, PutMode, PutOptions, PutPayload, UpdateVersion};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(crate) struct ScratchScope {
    archive: ArchiveStore,
    key: ArchiveStagingKey,
    database: Option<Database>,
    lease: Arc<tokio::sync::Mutex<Option<ArchiveStagingWriteLease>>>,
    fenced: Arc<AtomicBool>,
    uncertain: AtomicBool,
    heartbeat: Option<tokio::task::JoinHandle<()>>,
}
impl ScratchScope {
    pub(crate) async fn begin(
        archive: ArchiveStore,
        database: Database,
        key: ArchiveStagingKey,
    ) -> Result<Self, AppError> {
        let key = archive.select_staging_key(key)?;
        let lease = match database
            .begin_archive_staging_attempt(BeginArchiveStagingInput {
                key,
                intent_digest: ArchiveStagingIntentDigest::new(
                    blake3::hash(b"compact-scratch-pages-v1")
                        .to_hex()
                        .to_string(),
                )?,
                lease_token: uuid::Uuid::new_v4(),
                lease_owner: ArchiveStagingLeaseOwner::new(format!("compact:{}", key.attempt_id))?,
            })
            .await?
        {
            BeginArchiveStagingResult::Created(lease)
            | BeginArchiveStagingResult::Replayed(lease) => lease,
            BeginArchiveStagingResult::Existing(_) => {
                return Err(AppError::storage("compact_scratch_owner_conflict"));
            }
        };
        let key = lease.key;
        let lease = Arc::new(tokio::sync::Mutex::new(Some(lease)));
        let fenced = Arc::new(AtomicBool::new(false));
        let heartbeat_lease = lease.clone();
        let heartbeat_fenced = fenced.clone();
        let db = database.clone();
        let heartbeat = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(
                    ARCHIVE_STAGING_WRITE_HEARTBEAT_MILLIS as u64,
                ))
                .await;
                let mut guard = heartbeat_lease.lock().await;
                let Some(lease) = guard.as_mut() else {
                    return;
                };
                match db.heartbeat_archive_staging_write(lease).await {
                    Ok(true) => {}
                    _ => {
                        heartbeat_fenced.store(true, Ordering::Release);
                        return;
                    }
                }
            }
        });
        Ok(Self {
            archive,
            key,
            database: Some(database),
            lease,
            fenced,
            uncertain: AtomicBool::new(false),
            heartbeat: Some(heartbeat),
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(archive: ArchiveStore, key: ArchiveStagingKey) -> Self {
        Self {
            archive,
            key,
            database: None,
            lease: Arc::new(tokio::sync::Mutex::new(None)),
            fenced: Arc::new(AtomicBool::new(false)),
            uncertain: AtomicBool::new(false),
            heartbeat: None,
        }
    }

    fn locator(&self, page: u64) -> String {
        format!("{}/scratch/page-{page:016x}", self.key.canonical_prefix())
    }
    fn check(&self) -> Result<(), AppError> {
        if self.fenced.load(Ordering::Acquire) {
            Err(AppError::storage("compact_scratch_writer_fenced"))
        } else {
            Ok(())
        }
    }
    pub(crate) async fn read_page(&self, page: u64, output: &mut [u8]) -> Result<(), AppError> {
        self.check()?;
        let locator = self.locator(page);
        let (store, path) = self.archive.resolve(&locator)?;
        let result = store.get(&path).await?;
        if result.meta.size != output.len() as u64 {
            return Err(AppError::storage("compact_scratch_page_length"));
        }
        let mut stream = result.into_stream();
        let mut at = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if chunk.len() > output.len() - at {
                return Err(AppError::storage("compact_scratch_page_overrun"));
            }
            output[at..at + chunk.len()].copy_from_slice(&chunk);
            at += chunk.len();
        }
        if at != output.len() {
            return Err(AppError::storage("compact_scratch_page_truncated"));
        }
        self.check()
    }
    pub(crate) async fn write_page(&self, page: u64, bytes: &[u8]) -> Result<(), AppError> {
        self.check()?;
        if self.uncertain.load(Ordering::Acquire) {
            return Err(AppError::storage("compact_scratch_unknown_write"));
        }
        let locator = self.locator(page);
        crate::archive_staging::validate_bound_locator(self.key, &locator)?;
        let (store, path) = self.archive.resolve(&locator)?;
        let mode = match store.head(&path).await {
            Ok(meta) => {
                if meta.e_tag.is_none() && meta.version.is_none() {
                    return Err(AppError::storage(
                        "compact_scratch_conditional_write_missing",
                    ));
                }
                PutMode::Update(UpdateVersion {
                    e_tag: meta.e_tag,
                    version: meta.version,
                })
            }
            Err(object_store::Error::NotFound { .. }) => PutMode::Create,
            Err(error) => return Err(error.into()),
        };
        // An interrupted PUT never authorizes immediate abandonment or another
        // overwrite. The existing stale-writer grace and janitor retain ownership.
        self.uncertain.store(true, Ordering::Release);
        let result = store
            .put_opts(
                &path,
                PutPayload::from_bytes(Bytes::copy_from_slice(bytes)),
                PutOptions {
                    mode,
                    ..Default::default()
                },
            )
            .await;
        if let Err(error) = result {
            return Err(error.into());
        }
        self.uncertain.store(false, Ordering::Release);
        self.check()
    }
}
impl Drop for ScratchScope {
    fn drop(&mut self) {
        if let Some(task) = self.heartbeat.take() {
            task.abort();
        }
        if self.uncertain.load(Ordering::Acquire) {
            return;
        }
        let database = self.database.clone();
        let lease = self.lease.clone();
        let archive = self.archive.clone();
        let key = self.key;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Some(database) = database {
                    if let Some(lease) = lease.lock().await.take() {
                        let _ = database.abandon_archive_staging_attempt(&lease).await;
                    }
                } else {
                    let _ = archive.delete_archive_staging_segment(key).await;
                }
            });
        }
    }
}

#[cfg(test)]
impl ArchiveStore {
    pub(crate) fn with_read_delay_for_test(
        mut self,
    ) -> (
        Self,
        Arc<object_store::throttle::ThrottledStore<Arc<dyn object_store::ObjectStore>>>,
    ) {
        let store = Arc::new(object_store::throttle::ThrottledStore::new(
            self.inner.clone(),
            Default::default(),
        ));
        self.inner = store.clone();
        (self, store)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key() -> ArchiveStagingKey {
        let id = uuid::Uuid::new_v4();
        ArchiveStagingKey::new(
            ArchiveStagingOwner::ProxyRequest(id),
            ArchiveStagingPurpose::Response,
            id,
        )
        .unwrap()
    }
    #[tokio::test]
    async fn scratch_pages_use_conditional_replacement_and_cancelled_puts_remain_owned() {
        let (archive, store) = ArchiveStore::memory_for_test().with_read_delay_for_test();
        let scope = ScratchScope::for_test(archive.clone(), key());
        scope.write_page(7, b"first").await.unwrap();
        scope.write_page(7, b"later").await.unwrap();
        let mut bytes = [0; 5];
        scope.read_page(7, &mut bytes).await.unwrap();
        assert_eq!(&bytes, b"later");
        store.config_mut(|config| config.wait_put_per_call = std::time::Duration::from_secs(60));
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                scope.write_page(7, b"never")
            )
            .await
            .is_err()
        );
        assert!(scope.uncertain.load(Ordering::Acquire));
        assert!(scope.write_page(7, b"again").await.is_err());
        let key = scope.key;
        drop(scope);
        // Unknown publication does not authorize immediate scope deletion.
        assert!(!archive.archive_staging_segment_is_empty(key).await.unwrap());
        store.config_mut(|config| config.wait_put_per_call = std::time::Duration::ZERO);
    }
}
