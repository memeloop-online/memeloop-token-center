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
    archives: AtomicU64,
}

impl Default for Persistence {
    fn default() -> Self {
        Self {
            stream_memory: crate::gateway_body::memory::ProxyMemoryBudget::new(16 * 1024 * 1024),
            jobs: Arc::new(tokio::sync::Semaphore::new(JOBS)),
            bytes: Arc::new(tokio::sync::Semaphore::new(BYTES)),
            accepted: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            archives: AtomicU64::new(0),
        }
    }
}

impl Persistence {
    pub(crate) fn record_capacity_gap(&self) {
        self.rejected.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_failure_gap(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn render_routing(&self) -> String {
        self.render()
            .split("# TYPE memeloop_token_center_deferred_stream_memory_bytes")
            .next()
            .unwrap_or_default()
            .replace("deferred_persistence", "routing_persistence")
    }

    #[cfg(test)]
    pub(crate) async fn drain_for_test(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.archives.load(Ordering::Acquire) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all accepted archive bodies must finish their handoff");
        let _permits =
            tokio::time::timeout(Duration::from_secs(5), self.jobs.acquire_many(JOBS as u32))
                .await
                .expect("bounded buffered persistence drain")
                .unwrap();
    }

    pub(crate) fn submit(
        self: &Arc<Self>,
        bytes: usize,
        operation: impl std::future::Future<Output = Result<(), AppError>> + Send + 'static,
    ) -> bool {
        self.submit_with_headroom(bytes, 0, operation)
    }

    pub(crate) fn submit_observation(
        self: &Arc<Self>,
        bytes: usize,
        operation: impl std::future::Future<Output = Result<(), AppError>> + Send + 'static,
    ) -> bool {
        self.submit_with_headroom(bytes, 1, operation)
    }

    fn submit_with_headroom(
        self: &Arc<Self>,
        bytes: usize,
        headroom: u32,
        operation: impl std::future::Future<Output = Result<(), AppError>> + Send + 'static,
    ) -> bool {
        let permits = u32::try_from(bytes)
            .ok()
            .filter(|bytes| *bytes as usize <= BYTES)
            .and_then(|bytes| {
                let mut jobs = self
                    .jobs
                    .clone()
                    .try_acquire_many_owned(1 + headroom)
                    .ok()?;
                let job = jobs.split(1)?;
                Some((job, self.bytes.clone().try_acquire_many_owned(bytes).ok()?))
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

pub(super) fn record_capture_failure(state: &AppState, identity: crate::db::ArchiveSpoolIdentity) {
    let background = state.clone();
    state.persistence.archives.fetch_add(1, Ordering::Release);
    tokio::spawn(async move {
        loop {
            match background
                .archive
                .publish_raw_capture_failure(identity.tenant_id, identity.request_id, "response")
                .await
            {
                Ok(()) => break,
                Err(error) => {
                    tracing::warn!(request_id = %identity.request_id, error_category = error.diagnostic_category(),
                        "raw response capture admission failure awaits durable evidence");
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        }
        background
            .persistence
            .archives
            .fetch_sub(1, Ordering::Release);
    });
}

pub(super) fn capture(
    state: &AppState,
    identity: crate::db::ArchiveSpoolIdentity,
    purpose: crate::response_archive_spool::BufferedArchivePurpose,
    body: Bytes,
    memory: Arc<crate::gateway_body::memory::ProxyMemoryReservation>,
) {
    let background = state.clone();
    state.persistence.accepted.fetch_add(1, Ordering::Relaxed);
    state.persistence.archives.fetch_add(1, Ordering::Release);
    tokio::spawn(async move {
        let _memory = memory;
        let _job = background
            .persistence
            .jobs
            .clone()
            .acquire_owned()
            .await
            .expect("archive capture semaphore stays open");
        let mut durable = None;
        let mut db = background.persistence_db.clone();
        loop {
            db.refresh_closed_persistence_pool();
            let result: Result<bool, AppError> = async {
                let reference = match &durable {
                    Some(reference) => reference,
                    None => {
                        durable = Some(
                            background
                                .archive
                                .put_tenant_content(identity.tenant_id, body.clone())
                                .await?,
                        );
                        durable
                            .as_ref()
                            .expect("archive reference was just persisted")
                    }
                };
                background
                    .archive
                    .publish_raw_capture(
                        identity.tenant_id,
                        identity.request_id,
                        purpose.as_str(),
                        reference,
                    )
                    .await?;
                let _ = db
                    .bind_raw_capture(
                        identity.tenant_id,
                        identity.request_id,
                        purpose.as_str(),
                        reference,
                    )
                    .await?;
                Ok(true)
            }
            .await;
            match result {
                Ok(true) => break,
                Ok(false) => {}
                Err(error) => {
                    background
                        .persistence
                        .failed
                        .fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(request_id = %identity.request_id, error_category = error.diagnostic_category(),
                        "buffered archive capture retains original bytes for retry");
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        background
            .persistence
            .archives
            .fetch_sub(1, Ordering::Release);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn buffered_archive_waits_off_frontend_and_retries_without_losing_original_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("buffered-recovery.db").display()
        );
        let state = AppState::initialize(crate::config::Config::for_test(database_url.clone()))
            .await
            .unwrap();
        let pool = sqlx::AnyPool::connect(&database_url).await.unwrap();
        let identity = crate::db::ArchiveSpoolIdentity {
            request_id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            reservation_id: Uuid::new_v4(),
        };
        sqlx::query("INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, input_tokens, output_tokens, cost_micros, request_object, reservation_id) VALUES ($1, $2, $3, 1, 'openai', 'test', 0, 0, 0, 'inline-json:{}', $4)")
            .bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string())
            .bind(Uuid::new_v4().to_string()).bind(identity.reservation_id.to_string())
            .execute(&pool).await.unwrap();
        sqlx::query("UPDATE request_records SET completed_at = 2 WHERE id = $1")
            .bind(identity.request_id.to_string())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TRIGGER pause_buffered_capture BEFORE UPDATE OF response_object ON request_records BEGIN SELECT RAISE(ABORT, 'injected metadata outage'); END")
            .execute(&pool).await.unwrap();
        let jobs = state
            .persistence
            .jobs
            .clone()
            .acquire_many_owned(JOBS as u32)
            .await
            .unwrap();
        let memory = state.proxy_memory_budget.reservation();
        assert!(memory.reserve_buffered_response(64 * 1024, tokio::time::Instant::now()));
        let body = Bytes::from_static(br#"{"choices":[{"message":{"role":"assistant","content":"complete answer","tool_calls":[{"id":"call","type":"function","function":{"name":"search","arguments":"{}"}}]}}],"provider_field":"original"}"#);
        capture(
            &state,
            identity,
            crate::response_archive_spool::BufferedArchivePurpose::Response,
            body.clone(),
            memory,
        );
        assert_eq!(state.persistence.rejected.load(Ordering::Relaxed), 0);
        assert_eq!(state.persistence.archives.load(Ordering::Acquire), 1);
        assert!(state.proxy_memory_budget.snapshot().0 > 0);
        drop(jobs);
        tokio::time::timeout(Duration::from_secs(5), async {
            while state.persistence.failed.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(state.proxy_memory_budget.snapshot().0 > 0);
        sqlx::query("DROP TRIGGER pause_buffered_capture")
            .execute(&pool)
            .await
            .unwrap();
        state.persistence.drain_for_test().await;
        let reference = state
            .archive
            .raw_capture_reference(identity.tenant_id, identity.request_id, "response")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reference.blake3_digest,
            blake3::hash(&body).to_hex().to_string()
        );
        let encrypted_chunks: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM response_archive_spool_chunks")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(encrypted_chunks, 0);
        let locator: String =
            sqlx::query_scalar("SELECT response_object FROM request_records WHERE id = $1")
                .bind(identity.request_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state.archive.get(&locator).await.unwrap(), body);
        assert!(!crate::response_archive_spool::process_one_for_test(&state).await);
        let retained: i64 = sqlx::query_scalar(
            "SELECT cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(retained, 0);
        assert_eq!(state.proxy_memory_budget.snapshot().0, 0);
        pool.close().await;
        state.db.close().await;
    }

    #[tokio::test]
    async fn observations_leave_existing_health_work_a_job_without_waiting() {
        let persistence = Arc::new(Persistence::default());
        let mut releases = Vec::new();
        for _ in 0..JOBS - 1 {
            let (release, wait) = tokio::sync::oneshot::channel::<()>();
            releases.push(release);
            assert!(persistence.submit_observation(1024, async move {
                let _ = wait.await;
                Ok(())
            }));
        }
        assert!(!persistence.submit_observation(1024, async {
            panic!("observation must not consume health headroom")
        }));
        assert_eq!(persistence.jobs.available_permits(), 1);
        let (published, health) = tokio::sync::oneshot::channel();
        assert!(persistence.submit(4096, async move {
            published.send(()).unwrap();
            Ok(())
        }));
        health.await.unwrap();
        drop(releases);
        persistence.drain_for_test().await;
        assert_eq!(persistence.jobs.available_permits(), JOBS);
        assert_eq!(persistence.bytes.available_permits(), BYTES);
    }

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
        assert!(persistence.submit(BYTES, async {
            Err(AppError::internal(
                "persistence_deferred_persistence_releases_capacity_after_failure",
            ))
        }));
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
