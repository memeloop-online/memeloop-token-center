use super::*;
use crate::{db::Database, proxy_lifecycle::ProxyArchiveAttempt};

#[derive(Default)]
pub(super) struct ArchivePhase(std::sync::atomic::AtomicU8);

impl ArchivePhase {
    fn set(&self, phase: u8) {
        self.0.store(phase, std::sync::atomic::Ordering::Relaxed);
    }

    pub(super) fn writing(&self) {
        self.set(1);
    }
    pub(super) fn finishing(&self) {
        self.set(2);
    }
    pub(super) fn attaching(&self) {
        self.set(3);
    }

    fn label(&self) -> &'static str {
        match self.0.load(std::sync::atomic::Ordering::Relaxed) {
            1 => "write",
            2 => "finish",
            3 => "attach",
            _ => "start_writer",
        }
    }
}

pub(super) async fn run_bounded_proxy_lifecycle<F>(
    deadline: tokio::time::Instant,
    lifecycle: F,
) -> Result<F::Output, tokio::time::error::Elapsed>
where
    F: std::future::Future,
{
    tokio::time::timeout_at(deadline, lifecycle).await
}

pub(super) async fn run_bounded_text_archive<F, T>(
    deadline_millis: u32,
    request_id: uuid::Uuid,
    stage: &'static str,
    phase: &ArchivePhase,
    archive: F,
) -> Result<Result<T, AppError>, tokio::time::error::Elapsed>
where
    F: std::future::Future<Output = Result<T, AppError>>,
{
    let started = tokio::time::Instant::now();
    let result = tokio::time::timeout(Duration::from_millis(deadline_millis.into()), archive).await;
    let error_class = match &result {
        Ok(Ok(_)) => return result,
        Ok(Err(error)) => error.diagnostic_category(),
        Err(_) => "deadline_exceeded",
    };
    tracing::warn!(%request_id, stage, phase = phase.label(), error_class, elapsed_millis = started.elapsed().as_millis() as u64,
        deadline_millis, "proxy archive gap");
    result
}

pub(super) async fn finish_proxy_request_with_archive_fallback<'a>(
    database: &Database,
    input: FinishProxyRequest<'a>,
    archive_attempt: Option<&ProxyArchiveAttempt>,
    gap_response: &'a str,
) -> Result<FinishProxyRequestResult, AppError> {
    let stored_response = input.response_object;
    let primary = finish_proxy_request_with_retry(database, input.clone(), archive_attempt).await;
    if primary.is_ok() || archive_attempt.is_none() {
        if response_archive_requires_cleanup(&primary, stored_response)
            && let Some(attempt) = archive_attempt
        {
            abandon_proxy_archive_attempt(database, attempt).await;
        }
        return primary;
    }

    // An archive bind/fence failure must not turn a completed text response
    // into an availability failure. Re-finalizing with the original gap
    // locator is exactly-once: an unknown successful first commit replays its
    // stored locator, while a conclusive archive failure commits the gap.
    let fallback = finish_proxy_request_with_retry(
        database,
        FinishProxyRequest {
            response_object: gap_response,
            ..input
        },
        None,
    )
    .await;
    let cleanup = matches!(&fallback, Ok(FinishProxyRequestResult::Finished { .. }))
        || response_archive_requires_cleanup(&fallback, stored_response);
    if cleanup && let Some(attempt) = archive_attempt {
        abandon_proxy_archive_attempt(database, attempt).await;
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;

    // Virtual time models a slow store without introducing wall-clock sleeps or
    // widening the API suite's timeouts. Real writer/finalization is exercised.
    #[tokio::test(start_paused = true)]
    async fn archive_finishing_after_two_seconds_remains_available() {
        let config = crate::config::Config::for_test("sqlite::memory:".into());
        let store = crate::archive::ArchiveStore::from_config(&config)
            .await
            .unwrap();
        let phase = ArchivePhase::default();
        let result = run_bounded_text_archive(
            config.s3_timeouts.text_archive_millis,
            uuid::Uuid::nil(),
            "request_archive",
            &phase,
            async {
                let mut writer = store
                    .start_writer("staging/deadline-test/request.bin")
                    .await?;
                phase.writing();
                writer
                    .write(bytes::Bytes::from_static(b"archived request"))
                    .await?;
                phase.finishing();
                tokio::time::sleep(Duration::from_secs(3)).await;
                writer.finish_staged().await
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            store.get(&result.object_locator).await.unwrap().as_ref(),
            b"archived request"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn archive_deadline_returns_a_gap_outcome_and_cancels_work() {
        let phase = ArchivePhase::default();
        let completed = std::sync::atomic::AtomicBool::new(false);
        let started = tokio::time::Instant::now();
        let result = run_bounded_text_archive(
            5_000,
            uuid::Uuid::nil(),
            "buffered_response_archive",
            &phase,
            async {
                phase.finishing();
                tokio::time::sleep(Duration::from_secs(6)).await;
                completed.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok::<(), AppError>(())
            },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(started.elapsed(), Duration::from_secs(5));
        assert!(!completed.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(phase.label(), "finish");
    }
}
