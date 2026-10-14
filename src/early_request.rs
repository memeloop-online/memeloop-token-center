use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::sync::Notify;

use crate::db::{Database, EarlyRequestTerminal};

const MAX_RECORD_BYTES: u64 = 4096;
const MAX_PENDING_RECORDS: usize = 16_384;
const REPLAY_BATCH: usize = 64;

pub(crate) struct Journal {
    directory: PathBuf,
    wake: Arc<Notify>,
    writes: tokio::sync::Mutex<()>,
    written: AtomicU64,
    replayed: AtomicU64,
    failed: AtomicU64,
}

impl Journal {
    pub(crate) fn new(directory: PathBuf, database: Database) -> Arc<Self> {
        let journal = Arc::new(Self {
            directory,
            wake: Arc::new(Notify::new()),
            writes: tokio::sync::Mutex::new(()),
            written: AtomicU64::new(0),
            replayed: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        });
        let owner = Arc::downgrade(&journal);
        let wake = journal.wake.clone();
        tokio::spawn(async move {
            loop {
                let Some(journal) = owner.upgrade() else {
                    break;
                };
                journal.replay(&database).await;
                drop(journal);
                tokio::select! {
                    _ = wake.notified() => {},
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {},
                }
            }
        });
        journal
    }

    pub(crate) async fn append(&self, record: EarlyRequestTerminal) -> bool {
        let _writer = self.writes.lock().await;
        let directory = self.directory.clone();
        let request_id = record.request_id;
        let result = tokio::task::spawn_blocking(move || write_record(&directory, &record)).await;
        if matches!(result, Ok(Ok(()))) {
            self.written.fetch_add(1, Ordering::Relaxed);
            self.wake.notify_one();
            true
        } else {
            self.failed.fetch_add(1, Ordering::Relaxed);
            tracing::error!(%request_id, reason = "journal_unavailable", "early request terminal recording failed");
            false
        }
    }

    async fn replay(&self, database: &Database) {
        let Ok(mut entries) = tokio::fs::read_dir(&self.directory).await else {
            return;
        };
        let mut processed = 0;
        while let Ok(Some(entry)) = entries.next_entry().await {
            if processed >= REPLAY_BATCH {
                break;
            }
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            processed += 1;
            if !entry.file_type().await.is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let Ok(metadata) = entry.metadata().await else {
                continue;
            };
            if !metadata.is_file() || metadata.len() > MAX_RECORD_BYTES {
                self.failed.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let Ok(bytes) = tokio::fs::read(&path).await else {
                continue;
            };
            let Ok(record) = serde_json::from_slice::<EarlyRequestTerminal>(&bytes) else {
                self.failed.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            if path.file_stem().and_then(|value| value.to_str())
                != Some(record.request_id.to_string().as_str())
            {
                self.failed.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                database.record_early_request_terminal(&record),
            )
            .await;
            match result {
                Ok(Ok(())) => {
                    if tokio::fs::remove_file(&path).await.is_ok() {
                        self.replayed.fetch_add(1, Ordering::Relaxed);
                    }
                }
                _ => {
                    self.failed.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(request_id = %record.request_id, reason = "database_retry", "early request terminal retained for replay");
                    break;
                }
            }
        }
    }

    pub(crate) fn metrics(&self) -> String {
        format!(
            "# TYPE memeloop_token_center_early_terminal_total counter\nmemeloop_token_center_early_terminal_total{{outcome=\"journaled\"}} {}\nmemeloop_token_center_early_terminal_total{{outcome=\"replayed\"}} {}\nmemeloop_token_center_early_terminal_total{{outcome=\"failed\"}} {}\n",
            self.written.load(Ordering::Relaxed),
            self.replayed.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
        )
    }

    #[cfg(test)]
    pub(crate) async fn drain_for_test(&self) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                self.wake.notify_one();
                let mut pending = false;
                if let Ok(mut entries) = tokio::fs::read_dir(&self.directory).await {
                    while let Ok(Some(entry)) = entries.next_entry().await {
                        pending |= entry.path().extension().and_then(|value| value.to_str())
                            == Some("json");
                    }
                }
                if !pending {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("early terminal journal drained");
    }
}

fn write_record(directory: &Path, record: &EarlyRequestTerminal) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(std::io::Error::other("early terminal metadata limit"));
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(directory)?;
    let mut ancestor = PathBuf::new();
    for component in directory.components() {
        ancestor.push(component);
        if std::fs::symlink_metadata(&ancestor)?
            .file_type()
            .is_symlink()
        {
            return Err(std::io::Error::other("early terminal directory symlink"));
        }
    }
    if std::fs::read_dir(directory)?
        .take(MAX_PENDING_RECORDS)
        .count()
        >= MAX_PENDING_RECORDS
    {
        return Err(std::io::Error::other("early terminal journal capacity"));
    }
    let path = directory.join(format!("{}.json", record.request_id));
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    match temporary.persist_noclobber(&path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.error),
    }
    std::fs::File::open(directory)?.sync_all()?;
    Ok(())
}
