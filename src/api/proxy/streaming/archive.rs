use super::*;

/// A CR may be either a complete line ending or the first half of CRLF. Hold
/// only the already bounded archive batch until that single-byte ambiguity is
/// resolved, so the LF continuation is joined to its predecessor instead of
/// consuming a second archive channel slot.
#[derive(Default)]
pub(super) struct DeferredResponseArchive(Option<ResponseArchiveBatch>);

impl DeferredResponseArchive {
    pub(super) fn has_pending(&self) -> bool {
        self.0.is_some()
    }

    pub(super) fn queue(
        &mut self,
        sender: &tokio::sync::mpsc::Sender<ResponseArchiveBatch>,
        frames: &[SseDeliveryFrame],
        defer_for_crlf: bool,
        starts_with_crlf_continuation: bool,
    ) -> Result<(), ResponseArchiveBatchError> {
        let mut current = ResponseArchiveBatch::from_delivery_frames(frames)?;
        if let Some(mut deferred) = self.0.take() {
            if starts_with_crlf_continuation
                && let Some(current_batch) = current.as_mut()
                && current_batch
                    .chunks
                    .first()
                    .is_some_and(|bytes| bytes.as_ref() == b"\n")
            {
                let continuation = current_batch.chunks.remove(0);
                let total = deferred
                    .chunks
                    .iter()
                    .fold(0_usize, |total, bytes| total.saturating_add(bytes.len()));
                if total.saturating_add(continuation.len()) > MAX_PROXY_RESPONSE_BODY {
                    return Err(ResponseArchiveBatchError::BatchLimit);
                }
                let Some(last) = deferred.chunks.last_mut() else {
                    return Err(ResponseArchiveBatchError::BatchLimit);
                };
                let mut joined = Vec::with_capacity(last.len().saturating_add(continuation.len()));
                joined.extend_from_slice(last);
                joined.extend_from_slice(&continuation);
                *last = Bytes::from(joined);
            }
            try_send_response_archive_batch(sender, deferred)?;
        }
        if current
            .as_ref()
            .is_some_and(|current_batch| current_batch.chunks.is_empty())
        {
            current = None;
        }
        if defer_for_crlf {
            self.0 = current;
            return Ok(());
        }
        if let Some(current) = current {
            try_send_response_archive_batch(sender, current)?;
        }
        Ok(())
    }

    pub(super) fn flush(
        &mut self,
        sender: &tokio::sync::mpsc::Sender<ResponseArchiveBatch>,
    ) -> Result<(), ResponseArchiveBatchError> {
        if let Some(batch) = self.0.take() {
            try_send_response_archive_batch(sender, batch)?;
        }
        Ok(())
    }
}

pub(super) fn cancel_stream_archive(
    complete: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    sender: &mut Option<tokio::sync::mpsc::Sender<ResponseArchiveBatch>>,
) {
    complete.store(false, std::sync::atomic::Ordering::Release);
    drop(sender.take());
}

pub(super) async fn stream_response_archive(
    state: AppState,
    request_id: Uuid,
    archive_stream_permit: tokio::sync::OwnedSemaphorePermit,
    mut receiver: tokio::sync::mpsc::Receiver<ResponseArchiveBatch>,
    complete: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> (Option<crate::proxy_lifecycle::ProxyArchiveAttempt>, String) {
    // This sidecar is deliberately independent from downstream delivery. Its
    // bounded channel may fail closed into a gap, but can never backpressure
    // the client-facing SSE stream.
    let _archive_stream_permit = archive_stream_permit;
    let _archive_memory = state.metrics.memory_usage(
        crate::metrics::MemoryComponent::ArchiveMultipart,
        crate::archive::ARCHIVE_MULTIPART_PART_BYTES,
    );
    let gap = format!("gap://{request_id}/response");
    let (mut attempt, writer) = begin_streaming_response_archive(&state, request_id).await;
    let Some(mut writer) = writer else {
        return (None, gap);
    };
    let (archive_lease_lost_sender, mut archive_lease_lost_receiver) =
        tokio::sync::mpsc::channel(1);
    let mut archive_heartbeat_task = attempt.clone().map(|mut heartbeat_attempt| {
        let heartbeat_database = state.db.clone();
        AbortTaskOnDrop::new(tokio::spawn(async move {
            let mut heartbeat = tokio::time::interval(Duration::from_millis(
                u64::try_from(ARCHIVE_STAGING_WRITE_HEARTBEAT_MILLIS).unwrap_or(20_000),
            ));
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            heartbeat.tick().await;
            loop {
                heartbeat.tick().await;
                if !heartbeat_proxy_archive_attempt(&heartbeat_database, &mut heartbeat_attempt)
                    .await
                    .unwrap_or(false)
                {
                    let _ = archive_lease_lost_sender.send(()).await;
                    break;
                }
            }
        }))
    });
    let mut archive_failed = false;
    loop {
        let chunk = tokio::select! {
            biased;
            _ = archive_lease_lost_receiver.recv() => {
                tracing::warn!(%request_id, stage = "response_archive_heartbeat", "proxy archive gap");
                archive_failed = true;
                None
            }
            chunk = receiver.recv() => chunk,
        };
        let Some(batch) = chunk else {
            break;
        };
        if !complete.load(std::sync::atomic::Ordering::Acquire) {
            archive_failed = true;
            break;
        }
        for chunk in batch.chunks {
            match run_bounded_text_archive(writer.write(chunk)).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => {
                    tracing::warn!(%request_id, stage = "response_archive_stream", "proxy archive gap");
                    archive_failed = true;
                    break;
                }
            }
        }
        if archive_failed {
            break;
        }
    }
    if !complete.load(std::sync::atomic::Ordering::Acquire) {
        archive_failed = true;
    }
    if archive_failed {
        // Dropping the writer schedules a best-effort multipart abort without
        // adding a second object-store wait to this failed attempt. The fenced
        // staging row remains the durable cleanup source of truth.
        drop(writer);
        if let Some(current) = attempt.take() {
            abandon_proxy_archive_attempt(&state.db, &current).await;
        }
        return (None, gap);
    }
    let stored = match run_bounded_text_archive(writer.finish_staged()).await {
        Ok(Ok(staged))
            if attempt
                .as_ref()
                .is_some_and(|current| current.object_locator == staged.object_locator) =>
        {
            staged.object_locator
        }
        Ok(Ok(_)) | Ok(Err(_)) | Err(_) => {
            if let Some(current) = attempt.take() {
                abandon_proxy_archive_attempt(&state.db, &current).await;
            }
            tracing::warn!(%request_id, stage = "response_archive_finish", "proxy archive gap");
            return (None, gap);
        }
    };
    if let Some(task) = archive_heartbeat_task.as_mut() {
        task.abort();
    }
    (attempt, stored)
}
