use std::time::Duration;

use tokio::sync::watch;
use uuid::Uuid;

use crate::{
    AppState,
    archive_staging::ArchiveStagingPurpose,
    db::ArchiveSpoolTask,
    error::AppError,
    proxy_lifecycle::{begin_proxy_archive_attempt, heartbeat_proxy_archive_attempt},
};

const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) async fn run(state: AppState, mut shutdown: watch::Receiver<bool>) {
    let owner = Uuid::now_v7();
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    while !*shutdown.borrow() {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
            }
            _ = interval.tick() => {
                tokio::select! {
                    biased;
                    _ = shutdown.changed() => break,
                    _ = async {
                        // Bound each drain, but do not throttle a healthy store
                        // to one completed request per second.
                        for _ in 0..32 {
                            if !process_one(&state, owner).await { break; }
                        }
                    } => {}
                }
            }
        }
    }
}

pub(super) async fn process_one(state: &AppState, owner: Uuid) -> bool {
    // Stop at a committed transaction boundary instead of cancelling a live
    // SQL future. The latter can race SQLx's asynchronous rollback with the
    // next pooled BEGIN and generate transaction-state protocol notices.
    if state
        .db
        .cleanup_response_archive_spools_for(32, Duration::from_secs(2))
        .await
        .is_err()
    {
        tracing::warn!(
            stage = "response_spool_cleanup",
            "response archive cleanup failed; committed batches are preserved"
        );
    }
    let Ok(_permit) = state
        .proxy_archive_stream_permits
        .clone()
        .try_acquire_owned()
    else {
        return false;
    };
    let task = match tokio::time::timeout(
        Duration::from_secs(2),
        state.db.claim_response_archive_spool(owner),
    )
    .await
    {
        Ok(Ok(Some(task))) => task,
        Ok(Ok(None)) => return false,
        _ => {
            tracing::warn!(
                stage = "response_spool_claim",
                "response archive worker unavailable"
            );
            return false;
        }
    };
    let success = matches!(
        tokio::time::timeout(UPLOAD_TIMEOUT, upload(state, &task)).await,
        Ok(Ok(()))
    );
    if !success {
        // An upload/commit ACK may have been lost. Leave staged object cleanup
        // to the existing fenced reaper, which proves it unreferenced first.
        // retry() itself is fenced: a committed bind must never be undone.
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            state
                .db
                .retry_response_archive_spool(&task, "upload_failed"),
        )
        .await;
        tracing::warn!(request_id = %task.identity.request_id, stage = "response_spool_upload", "durable response archive retry pending");
    }
    true
}

async fn upload(state: &AppState, task: &ArchiveSpoolTask) -> Result<(), AppError> {
    let _archive_memory = state.metrics.memory_usage(
        crate::metrics::MemoryComponent::ArchiveMultipart,
        crate::archive::ARCHIVE_MULTIPART_PART_BYTES + 1024 * 1024 + super::CHUNK_BYTES * 5,
    );
    let attempt = begin_proxy_archive_attempt(
        &state.db,
        task.identity.request_id,
        ArchiveStagingPurpose::Response,
    )
    .await?;
    let heartbeat_state = state.clone();
    let heartbeat_task = task.clone();
    let mut heartbeat_attempt = attempt.clone();
    // Poll the heartbeat in the upload future itself. Cancelling/joining the
    // spool role now also drops this lease loop; there is no detached child.
    let heartbeat = async move {
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let renewed = tokio::time::timeout(Duration::from_secs(2), async {
                Ok::<_, AppError>(
                    heartbeat_state
                        .db
                        .heartbeat_response_archive_spool(&heartbeat_task)
                        .await?
                        && heartbeat_proxy_archive_attempt(
                            &heartbeat_state.db,
                            &mut heartbeat_attempt,
                        )
                        .await?,
                )
            })
            .await;
            if !matches!(renewed, Ok(Ok(true))) {
                break;
            }
        }
    };
    let transfer = async {
        let mut writer = state.archive.start_writer(&attempt.object_locator).await?;
        let mut total = 0_i64;
        let mut seq = 0;
        while seq < task.chunk_count {
            let chunks = state
                .db
                .load_response_archive_spool_batch(task, seq)
                .await?;
            if chunks.is_empty() {
                return Err(AppError::Internal);
            }
            for chunk in chunks {
                if chunk.seq != seq || seq >= task.chunk_count {
                    return Err(AppError::Internal);
                }
                let bytes = super::cipher::open(
                    task.identity,
                    seq,
                    &chunk.ciphertext,
                    chunk.byte_count,
                    state.config.key_pepper.as_bytes(),
                )?;
                total = total
                    .checked_add(chunk.byte_count)
                    .ok_or(AppError::Internal)?;
                writer.write(bytes).await?;
                seq += 1;
            }
        }
        if total != task.byte_count {
            return Err(AppError::Internal);
        }
        let stored = writer.finish_staged().await?;
        if stored.object_locator != attempt.object_locator
            || stored.size_bytes != u64::try_from(total).map_err(|_| AppError::Internal)?
        {
            return Err(AppError::Internal);
        }
        if !state
            .db
            .complete_response_archive_spool(task, &attempt.lease, &stored.object_locator)
            .await?
        {
            return Err(AppError::Internal);
        }
        Ok(())
    };
    tokio::select! {
        biased;
        _ = heartbeat => Err(AppError::Internal),
        result = transfer => result,
    }
}
