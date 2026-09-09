use super::*;
use crate::{db::Database, proxy_lifecycle::ProxyArchiveAttempt};

pub(super) const MAX_PROXY_TEXT_ARCHIVE_WAIT: Duration = Duration::from_secs(2);

pub(super) async fn run_bounded_proxy_lifecycle<F>(
    deadline: tokio::time::Instant,
    lifecycle: F,
) -> Result<F::Output, tokio::time::error::Elapsed>
where
    F: std::future::Future,
{
    tokio::time::timeout_at(deadline, lifecycle).await
}

pub(super) async fn run_bounded_text_archive<F>(
    archive: F,
) -> Result<F::Output, tokio::time::error::Elapsed>
where
    F: std::future::Future,
{
    tokio::time::timeout(MAX_PROXY_TEXT_ARCHIVE_WAIT, archive).await
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
