use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

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
// After a healthy process observes its first failed canary, it keeps serving
// for this bounded window while retrying. The clock starts when failure is
// observed, not when the preceding four-to-five-minute success cache began.
// Startup has no prior success and always fails closed immediately.
const READINESS_FAILURE_GRACE: Duration = Duration::from_secs(3 * 60);
// A cross-node S3/MinIO canary performs list, put, get and delete operations.
// Give that bounded sequence enough time to survive ordinary network jitter,
// while still failing a genuine storage outage before the outer readiness and
// Kubernetes probe deadlines.
const READINESS_DEADLINE: Duration = Duration::from_secs(5);
const READINESS_CANARY: &[u8] = b"memeloop-token-center/archive-readiness/v1";

#[derive(Clone, Copy)]
#[repr(u8)]
enum CanaryStage {
    Start = 0,
    List = 1,
    Put = 2,
    Get = 3,
    Read = 4,
    Delete = 5,
    Content = 6,
}

impl CanaryStage {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::List,
            2 => Self::Put,
            3 => Self::Get,
            4 => Self::Read,
            5 => Self::Delete,
            6 => Self::Content,
            _ => Self::Start,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::List => "list",
            Self::Put => "put",
            Self::Get => "get",
            Self::Read => "read",
            Self::Delete => "delete",
            Self::Content => "content",
        }
    }
}

#[derive(Clone)]
struct CanaryProgress(Arc<AtomicU8>);

impl CanaryProgress {
    fn new() -> Self {
        Self(Arc::new(AtomicU8::new(CanaryStage::Start as u8)))
    }

    fn enter(&self, stage: CanaryStage) {
        self.0.store(stage as u8, Ordering::Relaxed);
    }

    fn current(&self) -> CanaryStage {
        CanaryStage::from_u8(self.0.load(Ordering::Relaxed))
    }
}

struct CanaryFailure {
    stage: CanaryStage,
    timed_out: bool,
}

impl CanaryFailure {
    const fn operation(stage: CanaryStage) -> Self {
        Self {
            stage,
            timed_out: false,
        }
    }

    fn timeout(progress: &CanaryProgress) -> Self {
        Self {
            stage: progress.current(),
            timed_out: true,
        }
    }
}

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
        match (self.last_success_at, self.failure_grace_until) {
            (Some(_), None) => Ok(()),
            (Some(_), Some(valid_until)) if now <= valid_until => Ok(()),
            _ => Err(readiness_failure()),
        }
    }

    fn record_success(&mut self, now: tokio::time::Instant) {
        self.last_success_at = Some(now);
        let next_check_at = now + READINESS_REFRESH_BASE + self.refresh_jitter;
        self.next_check_at = Some(next_check_at);
        self.failure_grace_until = None;
    }

    fn record_failure(&mut self, now: tokio::time::Instant) -> Result<(), AppError> {
        if self.last_success_at.is_some() && self.failure_grace_until.is_none() {
            self.failure_grace_until = Some(now + READINESS_FAILURE_GRACE);
        }
        self.next_check_at = Some(now + READINESS_FAILURE_RETRY);
        self.effective_result(now)
    }
}

fn readiness_failure() -> AppError {
    AppError::Storage("archive readiness canary failed".to_owned())
}

impl ArchiveStore {
    pub async fn readiness_check(&self) -> Result<(), AppError> {
        let progress = CanaryProgress::new();
        self.readiness_check_with(
            READINESS_DEADLINE,
            progress.clone(),
            self.run_readiness_canary(progress),
        )
        .await
    }

    async fn readiness_check_with<F>(
        &self,
        deadline: Duration,
        progress: CanaryProgress,
        canary: F,
    ) -> Result<(), AppError>
    where
        F: Future<Output = Result<(), CanaryFailure>>,
    {
        let mut cache = self.readiness.lock().await;
        let now = tokio::time::Instant::now();
        if let Some(result) = cache.cached_result(now) {
            return result;
        }

        let check = tokio::time::timeout(deadline, canary)
            .await
            .unwrap_or_else(|_| Err(CanaryFailure::timeout(&progress)));
        let completed_at = tokio::time::Instant::now();
        match check {
            Ok(()) => {
                cache.record_success(completed_at);
                Ok(())
            }
            Err(failure) => {
                let effective = cache.record_failure(completed_at);
                let age = cache
                    .last_success_at
                    .map(|success| completed_at.duration_since(success))
                    .unwrap_or_default();
                tracing::warn!(
                    canary_stage = failure.stage.as_str(),
                    timed_out = failure.timed_out,
                    retaining_stale_success = effective.is_ok(),
                    stale_success_age_ms = age.as_millis() as u64,
                    failure_grace_remaining_ms = cache
                        .failure_grace_until
                        .map(
                            |until| until.saturating_duration_since(completed_at).as_millis()
                                as u64
                        )
                        .unwrap_or_default(),
                    "archive readiness canary failed"
                );
                effective
            }
        }
    }

    async fn run_readiness_canary(&self, progress: CanaryProgress) -> Result<(), CanaryFailure> {
        // List alone does not prove the application can archive and retrieve a
        // response. Exercise the exact read/write/delete permissions once at
        // startup and then cache the result so ordinary probes do not generate
        // continual object-store writes.
        // Restrict listing to the tiny operational prefix. Listing the archive
        // root turns a health check into a data-volume-dependent query.
        let readiness_prefix =
            archive_path("readiness").map_err(|_| CanaryFailure::operation(CanaryStage::Start))?;
        progress.enter(CanaryStage::List);
        let mut objects = self.inner.list(Some(&readiness_prefix));
        if let Some(first) = objects.next().await {
            first.map_err(|_| CanaryFailure::operation(CanaryStage::List))?;
        }
        progress.enter(CanaryStage::Put);
        self.inner
            .put(
                &self.readiness_path,
                PutPayload::from_static(READINESS_CANARY),
            )
            .await
            .map_err(|_| CanaryFailure::operation(CanaryStage::Put))?;
        progress.enter(CanaryStage::Get);
        let read = self
            .inner
            .get(&self.readiness_path)
            .await
            .map_err(|_| CanaryFailure::operation(CanaryStage::Get));
        let read = match read {
            Ok(read) => {
                progress.enter(CanaryStage::Read);
                read.bytes()
                    .await
                    .map_err(|_| CanaryFailure::operation(CanaryStage::Read))
            }
            Err(error) => Err(error),
        };
        progress.enter(CanaryStage::Delete);
        let delete = self.inner.delete(&self.readiness_path).await;
        let read = read?;
        delete.map_err(|_| CanaryFailure::operation(CanaryStage::Delete))?;
        if read.as_ref() != READINESS_CANARY {
            progress.enter(CanaryStage::Content);
            return Err(CanaryFailure::operation(CanaryStage::Content));
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

    async fn store_with_success_at_zero(refresh_jitter: Duration) -> ArchiveStore {
        let store = memory_store();
        store.readiness.lock().await.refresh_jitter = refresh_jitter;
        store
            .readiness_check_with(READINESS_DEADLINE, CanaryProgress::new(), async { Ok(()) })
            .await
            .expect("initial archive success");
        store
    }

    async fn timed_out_canary(store: &ArchiveStore) -> Result<(), AppError> {
        let store = store.clone();
        let timeout = tokio::spawn(async move {
            store
                .readiness_check_with(
                    READINESS_DEADLINE,
                    CanaryProgress::new(),
                    std::future::pending(),
                )
                .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(READINESS_DEADLINE).await;
        timeout.await.expect("timeout task")
    }

    #[test]
    fn archive_readiness_canary_has_a_bounded_five_second_deadline() {
        assert_eq!(READINESS_DEADLINE, Duration::from_secs(5));
        assert_eq!(READINESS_FAILURE_GRACE, Duration::from_secs(3 * 60));
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
    fn timeout_diagnostics_report_only_the_bounded_canary_stage() {
        let progress = CanaryProgress::new();
        progress.enter(CanaryStage::Read);
        let failure = CanaryFailure::timeout(&progress);
        assert_eq!(failure.stage.as_str(), "read");
        assert!(failure.timed_out);

        for value in 0..=u8::MAX {
            assert!(matches!(
                CanaryStage::from_u8(value).as_str(),
                "start" | "list" | "put" | "get" | "read" | "delete" | "content"
            ));
        }
    }

    #[test]
    fn a_short_failure_reuses_recent_success_and_retries_quickly() {
        let started = tokio::time::Instant::now();
        let mut cache = super::super::ReadinessCache {
            last_success_at: None,
            failure_grace_until: None,
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
            cache.failure_grace_until,
            Some(failed_at + READINESS_FAILURE_GRACE),
            "the full grace starts when failure is first observed"
        );
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
            failure_grace_until: None,
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
            failure_grace_until: None,
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
            failure_grace_until: None,
            next_check_at: None,
            refresh_jitter: Duration::ZERO,
        };
        persistent.record_success(started);
        let failed_at = started + READINESS_REFRESH_BASE;
        assert!(persistent.record_failure(failed_at).is_ok());
        let grace_boundary = failed_at + READINESS_FAILURE_GRACE;
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
        let refresh_jitter = Duration::from_secs(37);
        let store = store_with_success_at_zero(refresh_jitter).await;
        tokio::time::advance(READINESS_REFRESH_BASE + refresh_jitter).await;

        assert!(timed_out_canary(&store).await.is_ok());

        let completed_at = tokio::time::Instant::now();
        assert_eq!(
            store.readiness.lock().await.next_check_at,
            Some(completed_at + READINESS_FAILURE_RETRY)
        );
        assert_eq!(
            store.readiness.lock().await.failure_grace_until,
            Some(completed_at + READINESS_FAILURE_GRACE),
            "refresh TTL and jitter must not consume the failure grace"
        );

        tokio::time::advance(READINESS_FAILURE_RETRY - Duration::from_millis(1)).await;
        store
            .readiness_check_with(READINESS_DEADLINE, CanaryProgress::new(), async {
                panic!("cached stale success must not run a canary")
            })
            .await
            .expect("cached stale success");
        tokio::time::advance(Duration::from_millis(1)).await;
        store
            .readiness_check_with(READINESS_DEADLINE, CanaryProgress::new(), async { Ok(()) })
            .await
            .expect("scheduled retry");
        assert!(
            store.readiness.lock().await.failure_grace_until.is_none(),
            "a successful retry resets degraded readiness"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_startup_canary_timeout_fails_closed_immediately() {
        let store = memory_store();
        assert!(timed_out_canary(&store).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_two_and_a_half_minute_canary_outage_recovers_without_withdrawing_readiness() {
        let refresh_jitter = Duration::from_secs(29);
        let store = store_with_success_at_zero(refresh_jitter).await;
        tokio::time::advance(READINESS_REFRESH_BASE + refresh_jitter).await;

        // Ten five-second timeouts separated by ten-second retry intervals
        // model the 150-second production incident. Every observation remains
        // ready, then the first recovered canary resets degraded state.
        for attempt in 0..10 {
            assert!(
                timed_out_canary(&store).await.is_ok(),
                "attempt {attempt} withdrew readiness during the bounded outage"
            );
            tokio::time::advance(READINESS_FAILURE_RETRY).await;
        }
        store
            .readiness_check_with(READINESS_DEADLINE, CanaryProgress::new(), async { Ok(()) })
            .await
            .expect("archive recovered after 150 seconds");
        assert!(store.readiness.lock().await.failure_grace_until.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_canary_timeouts_fail_closed_after_the_stale_grace() {
        let refresh_jitter = Duration::from_secs(41);
        let store = store_with_success_at_zero(refresh_jitter).await;
        tokio::time::advance(READINESS_REFRESH_BASE + refresh_jitter).await;

        // Each failed attempt consumes the five-second deadline and is then
        // retried ten seconds later. Attempt 12 completes exactly on the
        // three-minute boundary; attempt 13 crosses it and withdraws readiness.
        for attempt in 0..14 {
            let result = timed_out_canary(&store).await;
            if attempt < 13 {
                assert!(result.is_ok(), "attempt {attempt} exceeded grace early");
                tokio::time::advance(READINESS_FAILURE_RETRY).await;
            } else {
                assert!(result.is_err(), "persistent outage must fail closed");
            }
        }
    }
}
