use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::Duration,
};

use futures_util::StreamExt;
use object_store::{MultipartUpload, ObjectStore, ObjectStoreExt, PutPayload, path::Path};

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
// This is a bounded cached gate, not an instantaneous storage signal: with
// regular probes a new outage is detected after at most the five-minute TTL
// plus the configured canary deadline; stale success then expires after this
// fixed grace (plus probe scheduling). Retrying never extends the grace.
const READINESS_FAILURE_GRACE: Duration = Duration::from_secs(3 * 60);
// A cross-node S3/MinIO canary performs list, multipart, get and delete operations.
// Give that bounded sequence enough time to survive ordinary network jitter,
// while still failing a genuine storage outage before the outer readiness and
// Kubernetes probe deadlines.
#[cfg(test)]
const READINESS_DEADLINE: Duration = Duration::from_secs(5);
const READINESS_CANARY: &[u8] = b"memeloop-token-center/archive-readiness/v1";
const CLEANUP_DEADLINE: Duration = Duration::from_secs(5);
const CANARY_STAGE_COUNT: usize = 11;

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
    MultipartStart = 7,
    MultipartPart = 8,
    MultipartComplete = 9,
    Abort = 10,
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
            7 => Self::MultipartStart,
            8 => Self::MultipartPart,
            9 => Self::MultipartComplete,
            10 => Self::Abort,
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
            Self::MultipartStart => "multipart_start",
            Self::MultipartPart => "multipart_part",
            Self::MultipartComplete => "multipart_complete",
            Self::Abort => "abort",
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
    error_class: &'static str,
}

// Fixed labels only: never put endpoint, object key or underlying error text in
// logs/metrics. object_store does not expose portable DNS/connect/TTFB timings.
#[derive(Default)]
pub(super) struct ReadinessMetrics {
    attempts: [[AtomicU64; 3]; CANARY_STAGE_COUNT],
    operations: [[AtomicU64; 3]; CANARY_STAGE_COUNT],
    operation_elapsed_millis: [AtomicU64; CANARY_STAGE_COUNT],
    elapsed_millis: AtomicU64,
    cache_hits: AtomicU64,
}

impl ReadinessMetrics {
    fn observe(&self, stage: CanaryStage, outcome: usize, elapsed: Duration) {
        self.attempts[stage as usize][outcome].fetch_add(1, Ordering::Relaxed);
        self.elapsed_millis
            .fetch_add(elapsed.as_millis() as u64, Ordering::Relaxed);
    }

    fn render(&self) -> String {
        use std::fmt::Write;
        let mut output =
            String::from("# TYPE memeloop_token_center_archive_canary_total counter\n");
        for stage in 0..CANARY_STAGE_COUNT as u8 {
            for (outcome, label) in ["success", "operation_error", "deadline"]
                .iter()
                .enumerate()
            {
                let _ = writeln!(
                    output,
                    "memeloop_token_center_archive_canary_total{{stage=\"{}\",outcome=\"{label}\"}} {}",
                    CanaryStage::from_u8(stage).as_str(),
                    self.attempts[stage as usize][outcome].load(Ordering::Relaxed)
                );
            }
        }
        output.push_str("# TYPE memeloop_token_center_archive_canary_operation_total counter\n");
        for stage in 0..CANARY_STAGE_COUNT as u8 {
            for (outcome, label) in ["success", "operation_error", "cancelled"]
                .iter()
                .enumerate()
            {
                let _ = writeln!(
                    output,
                    "memeloop_token_center_archive_canary_operation_total{{operation=\"{}\",outcome=\"{label}\"}} {}",
                    CanaryStage::from_u8(stage).as_str(),
                    self.operations[stage as usize][outcome].load(Ordering::Relaxed)
                );
            }
        }
        output.push_str("# TYPE memeloop_token_center_archive_canary_operation_duration_seconds_total counter\n");
        for stage in 0..CANARY_STAGE_COUNT as u8 {
            let _ = writeln!(
                output,
                "memeloop_token_center_archive_canary_operation_duration_seconds_total{{operation=\"{}\"}} {}",
                CanaryStage::from_u8(stage).as_str(),
                self.operation_elapsed_millis[stage as usize].load(Ordering::Relaxed) as f64
                    / 1000.0
            );
        }
        let _ = writeln!(
            output,
            "# TYPE memeloop_token_center_archive_canary_duration_seconds_total counter\nmemeloop_token_center_archive_canary_duration_seconds_total {}",
            self.elapsed_millis.load(Ordering::Relaxed) as f64 / 1000.0
        );
        let _ = writeln!(
            output,
            "# TYPE memeloop_token_center_archive_canary_cache_hits_total counter\nmemeloop_token_center_archive_canary_cache_hits_total {}",
            self.cache_hits.load(Ordering::Relaxed)
        );
        output
    }
}

// A dropped operation may be the overall deadline or caller cancellation.
// Do not misreport both as a storage error or expose the underlying error text.
struct OperationObservation<'a> {
    metrics: &'a ReadinessMetrics,
    stage: CanaryStage,
    started: tokio::time::Instant,
    outcome: usize,
}

impl Drop for OperationObservation<'_> {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed().as_millis() as u64;
        self.metrics.operations[self.stage as usize][self.outcome].fetch_add(1, Ordering::Relaxed);
        self.metrics.operation_elapsed_millis[self.stage as usize]
            .fetch_add(elapsed, Ordering::Relaxed);
        tracing::info!(
            operation = self.stage.as_str(),
            outcome = ["success", "operation_error", "cancelled"][self.outcome],
            elapsed_ms = elapsed,
            "archive readiness operation finished"
        );
    }
}

async fn canary_operation<T>(
    progress: &CanaryProgress,
    metrics: &ReadinessMetrics,
    stage: CanaryStage,
    operation: impl Future<Output = object_store::Result<T>>,
) -> Result<T, CanaryFailure> {
    progress.enter(stage);
    let mut observation = OperationObservation {
        metrics,
        stage,
        started: tokio::time::Instant::now(),
        outcome: 2,
    };
    let result = operation.await;
    observation.outcome = usize::from(result.is_err());
    result.map_err(|error| CanaryFailure::storage(stage, error))
}

// Keep the upload alive across cancellation so failed parts/completion can
// still be aborted. Unique keys ensure late cleanup cannot delete a new probe.
struct CanaryCleanup {
    upload: Option<Box<dyn MultipartUpload>>,
    store: Arc<dyn ObjectStore>,
    path: Path,
    metrics: Arc<ReadinessMetrics>,
    delete_needed: bool,
}

impl Drop for CanaryCleanup {
    fn drop(&mut self) {
        let upload = self.upload.take();
        if upload.is_none() && !self.delete_needed {
            return;
        }
        let store = self.store.clone();
        let path = self.path.clone();
        let metrics = self.metrics.clone();
        let delete_needed = self.delete_needed;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let progress = CanaryProgress::new();
                if let Some(mut upload) = upload {
                    let _ = tokio::time::timeout(
                        CLEANUP_DEADLINE,
                        canary_operation(&progress, &metrics, CanaryStage::Abort, upload.abort()),
                    )
                    .await;
                }
                // Try delete even when abort failed or timed out: completion
                // may have reached the server before its response was lost.
                if delete_needed {
                    let _ = tokio::time::timeout(
                        CLEANUP_DEADLINE,
                        canary_operation(
                            &progress,
                            &metrics,
                            CanaryStage::Delete,
                            store.delete(&path),
                        ),
                    )
                    .await;
                }
            });
        }
    }
}

async fn finish_canary_upload(
    cleanup: &mut CanaryCleanup,
    progress: &CanaryProgress,
) -> Result<(), CanaryFailure> {
    let upload = cleanup.upload.as_mut().expect("started canary upload");
    // A single final part may be smaller than S3's minimum non-final part.
    canary_operation(
        progress,
        &cleanup.metrics,
        CanaryStage::MultipartPart,
        upload.put_part(PutPayload::from_static(READINESS_CANARY)),
    )
    .await?;
    cleanup.delete_needed = true;
    canary_operation(
        progress,
        &cleanup.metrics,
        CanaryStage::MultipartComplete,
        upload.complete(),
    )
    .await?;
    cleanup.upload = None;
    Ok(())
}

impl CanaryFailure {
    const fn operation(stage: CanaryStage) -> Self {
        Self {
            stage,
            timed_out: false,
            error_class: "operation_error",
        }
    }

    fn timeout(progress: &CanaryProgress) -> Self {
        Self {
            stage: progress.current(),
            timed_out: true,
            error_class: "deadline",
        }
    }

    fn storage(stage: CanaryStage, error: object_store::Error) -> Self {
        let error_class = match error {
            object_store::Error::PermissionDenied { .. } => "permission_denied",
            object_store::Error::Unauthenticated { .. } => "unauthenticated",
            object_store::Error::NotFound { .. } => "not_found",
            // Generic includes transport and service failures; do not infer a
            // DNS/connect/TTFB cause by parsing possibly secret-bearing text.
            object_store::Error::Generic { .. } => "transport_or_service",
            _ => "operation_error",
        };
        Self {
            stage,
            timed_out: false,
            error_class,
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
    pub fn readiness_deadline(&self) -> Duration {
        self.readiness_deadline
    }

    pub fn readiness_metrics(&self) -> String {
        self.readiness_metrics.render()
    }

    pub async fn readiness_check(&self) -> Result<(), AppError> {
        let progress = CanaryProgress::new();
        self.readiness_check_with(
            self.readiness_deadline,
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
            self.readiness_metrics
                .cache_hits
                .fetch_add(1, Ordering::Relaxed);
            return result;
        }

        let check = tokio::time::timeout(deadline, canary)
            .await
            .unwrap_or_else(|_| Err(CanaryFailure::timeout(&progress)));
        let completed_at = tokio::time::Instant::now();
        let elapsed = completed_at.duration_since(now);
        match check {
            Ok(()) => {
                self.readiness_metrics
                    .observe(CanaryStage::Content, 0, elapsed);
                tracing::info!(
                    canary_stage = "complete",
                    elapsed_ms = elapsed.as_millis() as u64,
                    deadline_ms = deadline.as_millis() as u64,
                    "archive readiness canary succeeded"
                );
                cache.record_success(completed_at);
                Ok(())
            }
            Err(failure) => {
                self.readiness_metrics.observe(
                    failure.stage,
                    if failure.timed_out { 2 } else { 1 },
                    elapsed,
                );
                let effective = cache.record_failure(completed_at);
                let age = cache
                    .last_success_at
                    .map(|success| completed_at.duration_since(success))
                    .unwrap_or_default();
                tracing::warn!(
                    canary_stage = failure.stage.as_str(),
                    timed_out = failure.timed_out,
                    error_class = failure.error_class,
                    transport_phase = "opaque",
                    elapsed_ms = elapsed.as_millis() as u64,
                    deadline_ms = deadline.as_millis() as u64,
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
        canary_operation(
            &progress,
            &self.readiness_metrics,
            CanaryStage::List,
            async {
                let mut objects = self.inner.list(Some(&readiness_prefix));
                if let Some(first) = objects.next().await {
                    first?;
                }
                Ok(())
            },
        )
        .await?;
        let path = self.readiness_path.child(uuid::Uuid::now_v7().to_string());
        // If creation reaches S3 but is cancelled before returning its upload
        // handle, object_store cannot abort the unknown upload ID. Deployment
        // MUST verify provider-supported incomplete-multipart reclamation
        // (bucket lifecycle or MinIO global stale-upload cleanup); this gate
        // cannot verify that provider-specific policy via ObjectStore.
        let upload = canary_operation(
            &progress,
            &self.readiness_metrics,
            CanaryStage::MultipartStart,
            self.inner.put_multipart(&path),
        )
        .await?;
        let mut cleanup = CanaryCleanup {
            upload: Some(upload),
            store: self.inner.clone(),
            path,
            metrics: self.readiness_metrics.clone(),
            delete_needed: false,
        };
        finish_canary_upload(&mut cleanup, &progress).await?;
        let read = canary_operation(
            &progress,
            &self.readiness_metrics,
            CanaryStage::Get,
            self.inner.get(&cleanup.path),
        )
        .await?;
        let read = canary_operation(
            &progress,
            &self.readiness_metrics,
            CanaryStage::Read,
            read.bytes(),
        )
        .await?;
        progress.enter(CanaryStage::Content);
        let mut content = OperationObservation {
            metrics: &self.readiness_metrics,
            stage: CanaryStage::Content,
            started: tokio::time::Instant::now(),
            outcome: 0,
        };
        if read.as_ref() != READINESS_CANARY {
            content.outcome = 1;
            return Err(CanaryFailure::operation(CanaryStage::Content));
        }
        drop(content);
        canary_operation(
            &progress,
            &self.readiness_metrics,
            CanaryStage::Delete,
            self.inner.delete(&cleanup.path),
        )
        .await?;
        cleanup.delete_needed = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::AtomicUsize};

    use futures_util::FutureExt;
    use object_store::memory::InMemory;
    use object_store::{Extensions, PutResult, UploadPart};

    use super::*;

    #[derive(Debug)]
    struct FaultUpload {
        failure: &'static str,
        aborts: Arc<AtomicUsize>,
    }

    fn injected_error() -> object_store::Error {
        object_store::Error::Generic {
            store: "canary-test",
            source: Box::new(std::io::Error::other(
                "secret-bearing error must not escape",
            )),
        }
    }

    #[async_trait::async_trait]
    impl MultipartUpload for FaultUpload {
        fn put_part(&mut self, _data: PutPayload) -> UploadPart {
            let failure = self.failure;
            async move {
                match failure {
                    "part" => Err(injected_error()),
                    "part_timeout" => std::future::pending().await,
                    _ => Ok(()),
                }
            }
            .boxed()
        }

        async fn complete(&mut self) -> object_store::Result<PutResult> {
            match self.failure {
                "complete" => Err(injected_error()),
                "complete_timeout" => std::future::pending().await,
                _ => Ok(PutResult {
                    e_tag: None,
                    version: None,
                    extensions: Extensions::new(),
                }),
            }
        }

        async fn abort(&mut self) -> object_store::Result<()> {
            self.aborts.fetch_add(1, Ordering::Relaxed);
            if self.failure == "abort_timeout" {
                std::future::pending().await
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn multipart_canary_reads_verifies_deletes_and_caches() {
        let store = memory_store();
        store.readiness_check().await.expect("multipart ready");
        store.readiness_check().await.expect("cached ready");
        assert!(store.inner.list(None).next().await.is_none());
        let metrics = store.readiness_metrics();
        for operation in [
            "multipart_start",
            "multipart_part",
            "multipart_complete",
            "get",
            "read",
            "content",
            "delete",
        ] {
            assert!(metrics.contains(&format!(
                "operation=\"{operation}\",outcome=\"success\"}} 1"
            )));
        }
        assert!(metrics.contains("archive_canary_cache_hits_total 1"));
        assert!(!metrics.contains("unit-test"));
    }

    #[tokio::test(start_paused = true)]
    async fn operation_failures_and_cancellation_report_only_fixed_stages() {
        let metrics = ReadinessMetrics::default();
        for stage in [
            CanaryStage::List,
            CanaryStage::MultipartStart,
            CanaryStage::Get,
            CanaryStage::Read,
            CanaryStage::Delete,
        ] {
            let progress = CanaryProgress::new();
            let failure =
                canary_operation::<()>(&progress, &metrics, stage, async { Err(injected_error()) })
                    .await
                    .err()
                    .expect("injected failure");
            assert_eq!(failure.stage.as_str(), stage.as_str());
            assert_eq!(failure.error_class, "transport_or_service");
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(1),
                    canary_operation::<()>(&progress, &metrics, stage, std::future::pending())
                )
                .await
                .is_err()
            );
            assert_eq!(
                CanaryFailure::timeout(&progress).stage.as_str(),
                stage.as_str()
            );
            assert_eq!(
                metrics.operations[stage as usize][1].load(Ordering::Relaxed),
                1
            );
            assert_eq!(
                metrics.operations[stage as usize][2].load(Ordering::Relaxed),
                1
            );
        }
        assert!(!metrics.render().contains("secret-bearing"));
    }

    #[tokio::test(start_paused = true)]
    async fn multipart_failure_and_cancellation_abort_and_clean_ambiguous_completion() {
        for (failure, stage) in [
            ("part", CanaryStage::MultipartPart),
            ("complete", CanaryStage::MultipartComplete),
            ("part_timeout", CanaryStage::MultipartPart),
            ("complete_timeout", CanaryStage::MultipartComplete),
        ] {
            let store = memory_store();
            let aborts = Arc::new(AtomicUsize::new(0));
            let path = store.readiness_path.child(failure);
            // A completed object can exist despite a failed/lost complete response.
            if failure.starts_with("complete") {
                store
                    .inner
                    .put(&path, PutPayload::from_static(READINESS_CANARY))
                    .await
                    .expect("ambiguous object");
            }
            let mut cleanup = CanaryCleanup {
                upload: Some(Box::new(FaultUpload {
                    failure,
                    aborts: aborts.clone(),
                })),
                store: store.inner.clone(),
                path: path.clone(),
                metrics: store.readiness_metrics.clone(),
                delete_needed: false,
            };
            let progress = CanaryProgress::new();
            let result = tokio::time::timeout(
                Duration::from_millis(1),
                finish_canary_upload(&mut cleanup, &progress),
            )
            .await;
            if failure.ends_with("timeout") {
                assert!(result.is_err());
            } else {
                let failure = result
                    .expect("not a timeout")
                    .err()
                    .expect("operation failure");
                assert_eq!(failure.stage.as_str(), stage.as_str());
            }
            assert_eq!(progress.current().as_str(), stage.as_str());
            drop(cleanup);
            tokio::task::yield_now().await;
            assert_eq!(aborts.load(Ordering::Relaxed), 1);
            assert!(store.inner.get(&path).await.is_err());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stuck_abort_is_bounded_and_does_not_prevent_delete_or_touch_next_canary() {
        let store = memory_store();
        let old_path = store.readiness_path.child("old");
        let new_path = store.readiness_path.child("new");
        for path in [&old_path, &new_path] {
            store
                .inner
                .put(path, PutPayload::from_static(READINESS_CANARY))
                .await
                .expect("seed object");
        }
        let aborts = Arc::new(AtomicUsize::new(0));
        drop(CanaryCleanup {
            upload: Some(Box::new(FaultUpload {
                failure: "abort_timeout",
                aborts: aborts.clone(),
            })),
            store: store.inner.clone(),
            path: old_path.clone(),
            metrics: store.readiness_metrics.clone(),
            delete_needed: true,
        });
        tokio::task::yield_now().await;
        tokio::time::advance(CLEANUP_DEADLINE).await;
        tokio::task::yield_now().await;
        assert_eq!(aborts.load(Ordering::Relaxed), 1);
        assert!(store.inner.get(&old_path).await.is_err());
        assert!(store.inner.get(&new_path).await.is_ok());
        assert!(
            store
                .readiness_metrics()
                .contains("operation=\"abort\",outcome=\"cancelled\"} 1")
        );
    }

    #[tokio::test]
    async fn post_completion_failure_or_cancellation_deletes_without_abort() {
        let store = memory_store();
        let path = store.readiness_path.child("completed");
        store
            .inner
            .put(&path, PutPayload::from_static(READINESS_CANARY))
            .await
            .expect("completed object");
        // After successful completion, GET/read/content/delete failure or
        // cancellation drops this same state, with no upload left to abort.
        drop(CanaryCleanup {
            upload: None,
            store: store.inner.clone(),
            path: path.clone(),
            metrics: store.readiness_metrics.clone(),
            delete_needed: true,
        });
        tokio::task::yield_now().await;
        assert!(store.inner.get(&path).await.is_err());
        assert_eq!(
            store.readiness_metrics.operations[CanaryStage::Abort as usize][0]
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            store.readiness_metrics.operations[CanaryStage::Delete as usize][0]
                .load(Ordering::Relaxed),
            1
        );
    }

    #[tokio::test(start_paused = true)]
    async fn persistent_multipart_failures_expire_the_fixed_grace_even_on_a_cache_hit() {
        let store = store_with_success_at_zero(READINESS_REFRESH_JITTER_WINDOW).await;
        tokio::time::advance(READINESS_REFRESH_BASE + READINESS_REFRESH_JITTER_WINDOW).await;
        let first_failure_at = tokio::time::Instant::now();
        for elapsed in 0..=18 {
            assert!(
                store
                    .readiness_check_with(READINESS_DEADLINE, CanaryProgress::new(), async {
                        Err(CanaryFailure::operation(CanaryStage::MultipartPart))
                    })
                    .await
                    .is_ok()
            );
            assert_eq!(
                store.readiness.lock().await.failure_grace_until,
                Some(first_failure_at + READINESS_FAILURE_GRACE)
            );
            if elapsed < 18 {
                tokio::time::advance(READINESS_FAILURE_RETRY).await;
            }
        }
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(
            store
                .readiness_check_with(READINESS_DEADLINE, CanaryProgress::new(), async {
                    panic!("cached failure must expire grace without another object operation")
                })
                .await
                .is_err()
        );
    }

    fn memory_store() -> ArchiveStore {
        ArchiveStore {
            inner: Arc::new(InMemory::new()),
            readiness: Arc::new(tokio::sync::Mutex::new(
                super::super::ReadinessCache::default(),
            )),
            readiness_path: archive_path("readiness/unit-test.bin").expect("readiness path"),
            readiness_deadline: READINESS_DEADLINE,
            readiness_metrics: Arc::default(),
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
                "start"
                    | "list"
                    | "put"
                    | "get"
                    | "read"
                    | "delete"
                    | "content"
                    | "multipart_start"
                    | "multipart_part"
                    | "multipart_complete"
                    | "abort"
            ));
        }
    }

    #[test]
    fn storage_diagnostics_do_not_guess_transport_phase_or_expose_error_text() {
        let failure = CanaryFailure::storage(
            CanaryStage::List,
            object_store::Error::Generic {
                store: "secret-endpoint",
                source: std::io::Error::other("secret-url?credential=secret").into(),
            },
        );
        assert_eq!(failure.error_class, "transport_or_service");
        assert_eq!(failure.stage.as_str(), "list");
        assert!(
            !failure.timed_out,
            "an opaque source is not evidence of a deadline"
        );
        let failure = CanaryFailure::storage(
            CanaryStage::Put,
            object_store::Error::PermissionDenied {
                path: "secret-path".into(),
                source: std::io::Error::other("secret").into(),
            },
        );
        assert_eq!(failure.error_class, "permission_denied");
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
    async fn configured_deadline_is_observable_and_startup_recovers_after_retry_cache() {
        let mut store = memory_store();
        store.readiness_deadline = Duration::from_millis(750);
        assert_eq!(store.readiness_deadline(), Duration::from_millis(750));
        let progress = CanaryProgress::new();
        progress.enter(CanaryStage::List);
        assert!(
            store
                .readiness_check_with(store.readiness_deadline(), progress, std::future::pending())
                .await
                .is_err()
        );
        assert!(
            store
                .readiness_check_with(store.readiness_deadline(), CanaryProgress::new(), async {
                    panic!("failure cache must suppress a canary")
                })
                .await
                .is_err()
        );
        tokio::time::advance(READINESS_FAILURE_RETRY).await;
        store
            .readiness_check()
            .await
            .expect("recovered memory canary");
        let rendered = store.readiness_metrics();
        assert!(rendered.contains("stage=\"list\",outcome=\"deadline\"} 1"));
        assert!(rendered.contains("stage=\"content\",outcome=\"success\"} 1"));
        assert!(rendered.contains("archive_canary_cache_hits_total 1"));
        assert!(rendered.contains("archive_canary_duration_seconds_total 0.75"));
        assert!(!rendered.contains("unit-test.bin"));
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
