use super::*;

pub(super) fn cancel_stream_archive(
    complete: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    sender: &mut Option<tokio::sync::mpsc::Sender<Bytes>>,
) {
    complete.store(false, std::sync::atomic::Ordering::Release);
    drop(sender.take());
}

pub(super) async fn stream_response_archive(
    state: AppState,
    request_id: Uuid,
    archive_stream_permit: tokio::sync::OwnedSemaphorePermit,
    mut receiver: tokio::sync::mpsc::Receiver<Bytes>,
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
        let Some(chunk) = chunk else {
            break;
        };
        if !complete.load(std::sync::atomic::Ordering::Acquire) {
            archive_failed = true;
            break;
        }
        match run_bounded_text_archive(writer.write(chunk)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) | Err(_) => {
                tracing::warn!(%request_id, stage = "response_archive_stream", "proxy archive gap");
                archive_failed = true;
                break;
            }
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
