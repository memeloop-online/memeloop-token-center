use super::*;
use crate::proxy_lifecycle::ProxyArchiveAttempt;

pub(super) struct AbortTaskOnDrop<T>(Option<tokio::task::JoinHandle<T>>);

impl<T> AbortTaskOnDrop<T> {
    pub(super) fn new(task: tokio::task::JoinHandle<T>) -> Self {
        Self(Some(task))
    }

    pub(super) fn abort(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

impl<T> Drop for AbortTaskOnDrop<T> {
    fn drop(&mut self) {
        self.abort();
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

pub(super) async fn begin_streaming_response_archive(
    state: &AppState,
    request_id: Uuid,
) -> (
    Option<tokio::sync::OwnedSemaphorePermit>,
    Option<ProxyArchiveAttempt>,
    Option<crate::archive::ArchiveWriter>,
) {
    // The upstream response already exists, so this limits only multipart
    // archive buffers. Holding the permit through terminal finalization keeps
    // the memory bound intact while object-store completion is slow.
    let mut permit = match tokio::time::timeout(
        MAX_DOWNSTREAM_SEND_WAIT,
        state.proxy_archive_stream_permits.clone().acquire_owned(),
    )
    .await
    {
        Ok(Ok(permit)) => Some(permit),
        Ok(Err(_)) | Err(_) => {
            tracing::warn!(%request_id, stage = "response_archive_capacity", "proxy archive gap");
            None
        }
    };
    let mut attempt = if permit.is_some() {
        match begin_proxy_archive_attempt(&state.db, request_id, ArchiveStagingPurpose::Response)
            .await
        {
            Ok(attempt) => Some(attempt),
            Err(_) => {
                tracing::warn!(%request_id, stage = "response_archive_begin", "proxy archive gap");
                None
            }
        }
    } else {
        None
    };
    let writer = if let Some(current) = attempt.as_ref() {
        match state.archive.start_writer(&current.object_locator).await {
            Ok(writer) => Some(writer),
            Err(_) => {
                abandon_proxy_archive_attempt(&state.db, current).await;
                attempt = None;
                tracing::warn!(%request_id, stage = "response_archive", "proxy archive gap");
                None
            }
        }
    } else {
        None
    };
    if writer.is_none() {
        permit = None;
    }
    (permit, attempt, writer)
}
