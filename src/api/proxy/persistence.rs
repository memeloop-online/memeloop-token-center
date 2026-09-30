use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

const JOBS: usize = 4;
const BYTES: usize = 64 * 1024 * 1024;

pub(crate) struct Persistence {
    pub(crate) stream_memory: crate::gateway_body::memory::ProxyMemoryBudget,
    jobs: Arc<tokio::sync::Semaphore>,
    bytes: Arc<tokio::sync::Semaphore>,
    accepted: AtomicU64,
    rejected: AtomicU64,
    failed: AtomicU64,
}

impl Default for Persistence {
    fn default() -> Self {
        Self {
            stream_memory: crate::gateway_body::memory::ProxyMemoryBudget::new(4 * 1024 * 1024),
            jobs: Arc::new(tokio::sync::Semaphore::new(JOBS)),
            bytes: Arc::new(tokio::sync::Semaphore::new(BYTES)),
            accepted: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        }
    }
}

impl Persistence {
    pub(crate) fn submit(
        self: &Arc<Self>,
        bytes: usize,
        operation: impl std::future::Future<Output = Result<(), AppError>> + Send + 'static,
    ) -> bool {
        let permits = u32::try_from(bytes)
            .ok()
            .filter(|bytes| *bytes as usize <= BYTES)
            .and_then(|bytes| {
                Some((
                    self.jobs.clone().try_acquire_owned().ok()?,
                    self.bytes.clone().try_acquire_many_owned(bytes).ok()?,
                ))
            });
        let Some(permits) = permits else {
            self.rejected.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                stage = "deferred_persistence",
                outcome = "capacity",
                "proxy persistence gap"
            );
            return false;
        };
        self.accepted.fetch_add(1, Ordering::Relaxed);
        let counters = self.clone();
        tokio::spawn(async move {
            let _permits = permits;
            if let Err(error) = operation.await {
                counters.failed.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    stage = "deferred_persistence",
                    outcome = "failed",
                    error_category = error.diagnostic_category(),
                    "proxy persistence gap"
                );
            }
        });
        true
    }

    pub(crate) fn render(&self) -> String {
        let (stream_used, stream_limit, _, _) = self.stream_memory.snapshot();
        let mut output = format!(
            "# TYPE memeloop_token_center_deferred_persistence_jobs gauge\nmemeloop_token_center_deferred_persistence_jobs {}\n# TYPE memeloop_token_center_deferred_persistence_bytes gauge\nmemeloop_token_center_deferred_persistence_bytes {}\n# TYPE memeloop_token_center_deferred_persistence_total counter\nmemeloop_token_center_deferred_persistence_total{{outcome=\"accepted\"}} {}\nmemeloop_token_center_deferred_persistence_total{{outcome=\"capacity\"}} {}\nmemeloop_token_center_deferred_persistence_total{{outcome=\"failed\"}} {}\n",
            JOBS - self.jobs.available_permits(),
            BYTES - self.bytes.available_permits(),
            self.accepted.load(Ordering::Relaxed),
            self.rejected.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
        );
        output.push_str(&format!(
            "# TYPE memeloop_token_center_deferred_stream_memory_bytes gauge\nmemeloop_token_center_deferred_stream_memory_bytes {}\n# TYPE memeloop_token_center_deferred_stream_memory_limit_bytes gauge\nmemeloop_token_center_deferred_stream_memory_limit_bytes {}\n",
            stream_used, stream_limit,
        ));
        output
    }
}

pub(super) fn capture(
    state: &AppState,
    identity: crate::db::ArchiveSpoolIdentity,
    purpose: crate::response_archive_spool::BufferedArchivePurpose,
    body: Bytes,
) {
    let background = state.clone();
    let nodes = crate::gateway_body::memory::JsonMemoryScanner::default().observe(&body);
    let charge = body
        .len()
        .saturating_mul(3)
        .saturating_add(nodes.saturating_mul(256))
        .saturating_add(4 * 1024 * 1024);
    state.persistence.submit(charge, async move {
        let db = &background.persistence_db;
        if purpose == crate::response_archive_spool::BufferedArchivePurpose::Request {
            if let Err(error) = db.publish_proxy_started_event(identity).await {
                tracing::warn!(request_id = %identity.request_id, stage = "deferred_started_event", error_category = error.diagnostic_category(), "request event publication failed");
            }
            if body.len() > 16 * 1024 * 1024 {
                background.metrics.record_request_archive_gap(crate::metrics::RequestArchiveGapReason::RetentionLimit);
                return db.record_deferred_request_retention_gap(identity, &body).await;
            }
        }
        let retained = archive_retention::prepare_json_body_if_valid(&body);
        let body = retained.as_ref().map_or_else(
            || body.clone(), |retained| archive_retention::encode_json_body(&body, retained),
        );
        drop(retained);
        let archive = BufferedArchive::new(
            identity, purpose, &body, background.config.key_pepper.as_bytes(),
            background.config.archive_spool_compression_enabled,
        )?;
        if !db.capture_deferred_archive(&archive).await?
            && purpose == crate::response_archive_spool::BufferedArchivePurpose::Request {
            background.metrics.record_request_archive_gap(crate::metrics::RequestArchiveGapReason::Capacity);
        }
        Ok(())
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deferred_persistence_has_no_waiting_jobs_when_saturated() {
        let persistence = Arc::new(Persistence::default());
        let gate = Arc::new(tokio::sync::Notify::new());
        for _ in 0..JOBS {
            let gate = gate.clone();
            assert!(persistence.submit(1024, async move {
                gate.notified().await;
                Ok(())
            }));
        }
        assert!(!persistence.submit(1024, async { panic!("rejected work must never run") }));
        assert_eq!(persistence.jobs.available_permits(), 0);
        assert_eq!(persistence.bytes.available_permits(), BYTES - JOBS * 1024);
        assert!(persistence.render().contains("outcome=\"capacity\"} 1"));
        tokio::task::yield_now().await;
        gate.notify_waiters();
        tokio::time::timeout(Duration::from_secs(1), async {
            while persistence.jobs.available_permits() != JOBS {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn deferred_persistence_releases_capacity_after_failure_and_rejects_oversize() {
        let persistence = Arc::new(Persistence::default());
        assert!(!persistence.submit(BYTES + 1, async { Ok(()) }));
        assert!(persistence.submit(BYTES, async { Err(AppError::Internal) }));
        tokio::time::timeout(Duration::from_secs(1), async {
            while persistence.jobs.available_permits() != JOBS {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(persistence.bytes.available_permits(), BYTES);
        assert!(persistence.render().contains("outcome=\"failed\"} 1"));
    }
}
