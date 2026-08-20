use std::{future::Future, time::Duration};

use futures_util::StreamExt;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

use super::{ArchiveStore, path::archive_path};
use crate::error::AppError;

// Successful canaries are refreshed between four and five minutes. A
// process-unique offset prevents Pods started by the same rollout from
// synchronising their S3 write canaries forever.
const READINESS_REFRESH_BASE: Duration = Duration::from_secs(4 * 60);
const READINESS_REFRESH_JITTER_WINDOW: Duration = Duration::from_secs(60);
const READINESS_FAILURE_RETRY: Duration = Duration::from_secs(10);
// A recently proven archive remains ready across a short failed canary. The
// check is retried during this grace period; a continuing failure becomes
// not-ready after one minute. Startup has no prior success and always fails
// closed immediately.
const READINESS_STALE_SUCCESS_GRACE: Duration = Duration::from_secs(60);
// A cross-node S3/MinIO canary performs list, put, get and delete operations.
// Give that bounded sequence enough time to survive ordinary network jitter,
// while still failing a genuine storage outage before the outer readiness and
// Kubernetes probe deadlines.
const READINESS_DEADLINE: Duration = Duration::from_secs(5);
const READINESS_CANARY: &[u8] = b"memeloop-token-center/archive-readiness/v1";

pub(super) fn refresh_jitter(seed: u64) -> Duration {
    let window_millis = READINESS_REFRESH_JITTER_WINDOW.as_millis() as u64;
    Duration::from_millis(seed % (window_millis + 1))
}

impl super::ReadinessCache {
    fn cached_result(&self, now: tokio::time::Instant) -> Option<Result<(), AppError>> {
        let next_check_at = self.next_check_at?;
        if now >= next_check_at {
            return None;
        }
        Some(self.effective_result(now))
    }

    fn effective_result(&self, now: tokio::time::Instant) -> Result<(), AppError> {
        if self
            .stale_success_until
            .is_some_and(|valid_until| now <= valid_until)
        {
            Ok(())
        } else {
            Err(readiness_failure())
        }
    }

    fn record_success(&mut self, now: tokio::time::Instant) {
        self.last_success_at = Some(now);
        let next_check_at = now + READINESS_REFRESH_BASE + self.refresh_jitter;
        self.next_check_at = Some(next_check_at);
        self.stale_success_until = Some(next_check_at + READINESS_STALE_SUCCESS_GRACE);
    }

    fn record_failure(&mut self, now: tokio::time::Instant) -> Result<(), AppError> {
        self.next_check_at = Some(now + READINESS_FAILURE_RETRY);
        self.effective_result(now)
    }
}

fn readiness_failure() -> AppError {
    AppError::Storage("archive readiness canary failed".to_owned())
}

impl ArchiveStore {
    pub async fn readiness_check(&self) -> Result<(), AppError> {
        self.readiness_check_with(READINESS_DEADLINE, self.run_readiness_canary())
            .await
    }

    async fn readiness_check_with<F>(&self, deadline: Duration, canary: F) -> Result<(), AppError>
    where
        F: Future<Output = Result<(), AppError>>,
    {
        let mut cache = self.readiness.lock().await;
        let now = tokio::time::Instant::now();
        if let Some(result) = cache.cached_result(now) {
            return result;
        }

        let check = tokio::time::timeout(deadline, canary)
            .await
            .unwrap_or_else(|_| {
                Err(AppError::Storage(
                    "archive readiness canary timed out".to_owned(),
                ))
            });
        let completed_at = tokio::time::Instant::now();
        match check {
            Ok(()) => {
                cache.record_success(completed_at);
                Ok(())
            }
            Err(_) => {
                let effective = cache.record_failure(completed_at);
                if effective.is_ok() {
                    let age = cache
                        .last_success_at
                        .map(|success| completed_at.duration_since(success))
                        .unwrap_or_default();
                    tracing::warn!(
                        stale_success_age_ms = age.as_millis() as u64,
                        stale_success_grace_ms = READINESS_STALE_SUCCESS_GRACE.as_millis() as u64,
                        "archive readiness canary failed; retaining bounded stale success"
                    );
                }
                effective
            }
        }
    }

    async fn run_readiness_canary(&self) -> Result<(), AppError> {
        // List alone does not prove the application can archive and retrieve a
        // response. Exercise the exact read/write/delete permissions once at
        // startup and then cache the result so ordinary probes do not generate
        // continual object-store writes.
        // Restrict listing to the tiny operational prefix. Listing the archive
        // root turns a health check into a data-volume-dependent query.
        let readiness_prefix = archive_path("readiness")?;
        let mut objects = self.inner.list(Some(&readiness_prefix));
        if let Some(first) = objects.next().await {
            first.map_err(|_| {
                AppError::Storage("archive readiness canary operation failed".to_owned())
            })?;
        }
        self.inner
            .put(
                &self.readiness_path,
                PutPayload::from_static(READINESS_CANARY),
            )
            .await
            .map_err(|_| {
                AppError::Storage("archive readiness canary operation failed".to_owned())
            })?;
        let read =
            self.inner.get(&self.readiness_path).await.map_err(|_| {
                AppError::Storage("archive readiness canary operation failed".to_owned())
            });
        let read = match read {
            Ok(read) => read.bytes().await.map_err(|_| {
                AppError::Storage("archive readiness canary operation failed".to_owned())
            }),
            Err(error) => Err(error),
        };
        let delete = self.inner.delete(&self.readiness_path).await;
        let read = read?;
        delete.map_err(|_| {
            AppError::Storage("archive readiness canary operation failed".to_owned())
        })?;
        if read.as_ref() != READINESS_CANARY {
            return Err(AppError::Storage(
                "archive readiness canary content mismatch".to_owned(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use object_store::memory::InMemory;

    use super::*;

    fn memory_store() -> ArchiveStore {
        ArchiveStore {
            inner: Arc::new(InMemory::new()),
            readiness: Arc::new(tokio::sync::Mutex::new(
                super::super::ReadinessCache::default(),
            )),
            readiness_path: archive_path("readiness/unit-test.bin").expect("readiness path"),
        }
    }

    async fn store_with_success_at_zero() -> ArchiveStore {
        let store = memory_store();
        store.readiness.lock().await.refresh_jitter = Duration::ZERO;
        store
            .readiness_check_with(READINESS_DEADLINE, async { Ok(()) })
            .await
            .expect("initial archive success");
        store
    }

    #[test]
    fn archive_readiness_canary_has_a_bounded_five_second_deadline() {
        assert_eq!(READINESS_DEADLINE, Duration::from_secs(5));
    }

    #[test]
    fn refresh_jitter_is_bounded_and_seeded() {
        assert_eq!(refresh_jitter(0), Duration::ZERO);
        assert_eq!(
            refresh_jitter(READINESS_REFRESH_JITTER_WINDOW.as_millis() as u64),
            READINESS_REFRESH_JITTER_WINDOW
        );
        assert_eq!(
            refresh_jitter(READINESS_REFRESH_JITTER_WINDOW.as_millis() as u64 + 1),
            Duration::ZERO
        );
    }

    #[test]
    fn a_short_failure_reuses_recent_success_and_retries_quickly() {
        let started = tokio::time::Instant::now();
        let mut cache = super::super::ReadinessCache {
            last_success_at: None,
            stale_success_until: None,
            next_check_at: None,
            refresh_jitter: Duration::from_secs(23),
        };
        cache.record_success(started);
        assert_eq!(
            cache.next_check_at,
            Some(started + READINESS_REFRESH_BASE + Duration::from_secs(23))
        );

        let failed_at = started + READINESS_REFRESH_BASE + Duration::from_secs(23);
        assert!(cache.record_failure(failed_at).is_ok());
        assert_eq!(
            cache.next_check_at,
            Some(failed_at + READINESS_FAILURE_RETRY)
        );
        assert!(
            cache
                .cached_result(failed_at + READINESS_FAILURE_RETRY - Duration::from_millis(1))
                .expect("failure retry cache")
                .is_ok()
        );
        assert!(
            cache
                .cached_result(failed_at + READINESS_FAILURE_RETRY)
                .is_none(),
            "the stale success must not suppress the scheduled retry"
        );
    }

    #[test]
    fn twenty_five_second_canary_outage_never_withdraws_readiness() {
        let started = tokio::time::Instant::now();
        let mut cache = super::super::ReadinessCache {
            last_success_at: None,
            stale_success_until: None,
            next_check_at: None,
            refresh_jitter: Duration::ZERO,
        };
        cache.record_success(started);
        let outage_started = started + READINESS_REFRESH_BASE;

        // Model the production incident: Kubernetes polls every five seconds,
        // while the failed canary is retried every ten. All six observations
        // spanning a 25-second S3 interruption must remain ready.
        for elapsed_seconds in [0, 5, 10, 15, 20, 25] {
            let now = outage_started + Duration::from_secs(elapsed_seconds);
            let result = match cache.cached_result(now) {
                Some(cached) => cached,
                None => cache.record_failure(now),
            };
            assert!(result.is_ok(), "readiness withdrew at {elapsed_seconds}s");
        }

        cache.record_success(outage_started + Duration::from_secs(30));
        assert!(
            cache
                .cached_result(outage_started + Duration::from_secs(35))
                .expect("recovered success cache")
                .is_ok()
        );
    }

    #[test]
    fn startup_and_persistent_archive_failures_fail_closed() {
        let started = tokio::time::Instant::now();
        let mut startup = super::super::ReadinessCache {
            last_success_at: None,
            stale_success_until: None,
            next_check_at: None,
            refresh_jitter: Duration::ZERO,
        };
        assert!(startup.record_failure(started).is_err());
        assert!(
            startup
                .cached_result(started + Duration::from_secs(1))
                .expect("startup failure cache")
                .is_err()
        );

        let mut persistent = super::super::ReadinessCache {
            last_success_at: None,
            stale_success_until: None,
            next_check_at: None,
            refresh_jitter: Duration::ZERO,
        };
        persistent.record_success(started);
        let grace_boundary = started + READINESS_REFRESH_BASE + READINESS_STALE_SUCCESS_GRACE;
        assert!(persistent.effective_result(grace_boundary).is_ok());
        assert!(
            persistent
                .record_failure(grace_boundary + Duration::from_millis(1))
                .is_err(),
            "a continuing outage must fail closed after the bounded grace"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_canary_uses_stale_success_and_retries_after_ten_seconds() {
        let store = store_with_success_at_zero().await;
        tokio::time::advance(READINESS_REFRESH_BASE).await;

        let store_for_timeout = store.clone();
        let timeout = tokio::spawn(async move {
            store_for_timeout
                .readiness_check_with(READINESS_DEADLINE, std::future::pending())
                .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(READINESS_DEADLINE).await;
        assert!(timeout.await.expect("timeout task").is_ok());

        let completed_at = tokio::time::Instant::now();
        assert_eq!(
            store.readiness.lock().await.next_check_at,
            Some(completed_at + READINESS_FAILURE_RETRY)
        );

        tokio::time::advance(READINESS_FAILURE_RETRY - Duration::from_millis(1)).await;
        store
            .readiness_check_with(READINESS_DEADLINE, async {
                panic!("cached stale success must not run a canary")
            })
            .await
            .expect("cached stale success");
        tokio::time::advance(Duration::from_millis(1)).await;
        store
            .readiness_check_with(READINESS_DEADLINE, async { Ok(()) })
            .await
            .expect("scheduled retry");
    }

    #[tokio::test(start_paused = true)]
    async fn a_startup_canary_timeout_fails_closed_immediately() {
        let store = memory_store();
        let store_for_timeout = store.clone();
        let timeout = tokio::spawn(async move {
            store_for_timeout
                .readiness_check_with(READINESS_DEADLINE, std::future::pending())
                .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(READINESS_DEADLINE).await;
        assert!(timeout.await.expect("timeout task").is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_canary_timeouts_fail_closed_after_the_stale_grace() {
        let store = store_with_success_at_zero().await;
        tokio::time::advance(READINESS_REFRESH_BASE).await;

        // Each failed attempt consumes the five-second deadline and is then
        // retried ten seconds later. Four completions remain within the
        // one-minute grace; the fifth crosses it and must withdraw readiness.
        for attempt in 0..5 {
            let store_for_timeout = store.clone();
            let timeout = tokio::spawn(async move {
                store_for_timeout
                    .readiness_check_with(READINESS_DEADLINE, std::future::pending())
                    .await
            });
            tokio::task::yield_now().await;
            tokio::time::advance(READINESS_DEADLINE).await;
            let result = timeout.await.expect("timeout task");
            if attempt < 4 {
                assert!(result.is_ok(), "attempt {attempt} exceeded grace early");
                tokio::time::advance(READINESS_FAILURE_RETRY).await;
            } else {
                assert!(result.is_err(), "persistent outage must fail closed");
            }
        }
    }
}
