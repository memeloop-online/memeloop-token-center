use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::sync::Notify;

use crate::db::{Database, EarlyRequestTerminal};

const MAX_RECORD_BYTES: u64 = 4096;
const REPLAY_BATCH: usize = 64;
const MAX_PENDING_WRITES: usize = 64;
const WRITE_DEADLINE: Duration = Duration::from_millis(250);

pub(crate) struct Journal {
    directory: PathBuf,
    database: Database,
    wake: Arc<Notify>,
    writes: Arc<tokio::sync::Mutex<()>>,
    pending_writes: Arc<tokio::sync::Semaphore>,
    written: Arc<AtomicU64>,
    replayed: AtomicU64,
    failed: AtomicU64,
}

impl Journal {
    pub(crate) fn new(directory: PathBuf, database: Database) -> Arc<Self> {
        let journal = Arc::new(Self {
            directory,
            database: database.clone(),
            wake: Arc::new(Notify::new()),
            writes: Arc::new(tokio::sync::Mutex::new(())),
            pending_writes: Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_WRITES)),
            written: Arc::new(AtomicU64::new(0)),
            replayed: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        });
        let owner = Arc::downgrade(&journal);
        let wake = journal.wake.clone();
        tokio::spawn(async move {
            while let Some(journal) = owner.upgrade() {
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
        let request_id = record.request_id;
        let Ok(_pending) = self.pending_writes.clone().try_acquire_owned() else {
            return self.unavailable(request_id, "journal_queue_full");
        };
        let database = self.database.clone();
        let writes = self.writes.clone();
        let written = self.written.clone();
        let result = tokio::time::timeout(WRITE_DEADLINE, async move {
            let _writer = writes.lock_owned().await;
            let result = database.record_early_request_terminal(&record).await;
            if result.is_ok() {
                written.fetch_add(1, Ordering::Relaxed);
            }
            result
        })
        .await;
        match result {
            Ok(Ok(())) => true,
            Err(_) => self.unavailable(request_id, "journal_write_timeout"),
            _ => self.unavailable(request_id, "journal_unavailable"),
        }
    }

    fn unavailable(&self, request_id: uuid::Uuid, reason: &'static str) -> bool {
        self.failed.fetch_add(1, Ordering::Relaxed);
        tracing::error!(%request_id, reason, "early request terminal recording failed");
        false
    }

    #[cfg(test)]
    pub(crate) async fn hold_writer_for_test(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.writes.clone().lock_owned().await
    }

    #[cfg(test)]
    pub(crate) fn pending_writes_for_test(&self) -> usize {
        MAX_PENDING_WRITES - self.pending_writes.available_permits()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn saturated_journal_queue_fails_without_spawning_or_publishing() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("queue.db").display()
        ))
        .await
        .unwrap();
        let path = directory.path().join("journal");
        let journal = Journal::new(path.clone(), database.clone());
        let held = journal
            .pending_writes
            .clone()
            .acquire_many_owned(MAX_PENDING_WRITES as u32)
            .await
            .unwrap();
        let now = crate::db::unix_millis();
        assert!(
            !journal
                .append(EarlyRequestTerminal {
                    request_id: uuid::Uuid::now_v7(),
                    tenant_id: uuid::Uuid::nil(),
                    key_id: uuid::Uuid::nil(),
                    created_at: now,
                    completed_at: now,
                    duration_ms: 0,
                    protocol: "openai".into(),
                    model: "synthetic".into(),
                    status_code: 503,
                    error_code: "no_authorized_upstream_candidate".into(),
                })
                .await
        );
        assert_eq!(journal.written.load(Ordering::Relaxed), 0);
        assert_eq!(journal.failed.load(Ordering::Relaxed), 1);
        assert!(!path.exists());
        drop(held);
        assert_eq!(journal.pending_writes_for_test(), 0);
        database.close().await;
    }
}
