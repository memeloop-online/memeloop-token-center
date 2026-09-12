use std::{future::Future, time::Duration};

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
const SLOW_CLAIM: Duration = Duration::from_secs(2);

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
                // Never cancel an in-flight claim/cleanup transaction on
                // shutdown. The database's acquire/lock/statement deadlines
                // bound SQL; shutdown is observed at transaction boundaries.
                drain_batch(&shutdown, || process_one_until_shutdown(&state, owner, Some(&shutdown))).await;
            }
        }
    }
}

async fn drain_batch<F, Fut>(shutdown: &watch::Receiver<bool>, mut process: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    // Bound each drain without throttling a healthy store to one item/second.
    for _ in 0..32 {
        if stopping(shutdown) || !process().await {
            break;
        }
    }
}

fn stopping(shutdown: &watch::Receiver<bool>) -> bool {
    *shutdown.borrow() || shutdown.has_changed().is_err()
}

#[cfg(test)]
pub(super) async fn process_one(state: &AppState, owner: Uuid) -> bool {
    process_one_until_shutdown(state, owner, None).await
}

async fn process_one_until_shutdown(
    state: &AppState,
    owner: Uuid,
    shutdown: Option<&watch::Receiver<bool>>,
) -> bool {
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
    if shutdown.is_some_and(stopping) {
        return false;
    }
    let Ok(_permit) = state
        .proxy_archive_stream_permits
        .clone()
        .try_acquire_owned()
    else {
        return false;
    };
    let task = match observe_claim(state.db.claim_response_archive_spool(owner)).await {
        Ok(Some(task)) => task,
        Ok(None) | Err(_) => return false,
    };
    if shutdown.is_some_and(stopping) {
        // No object I/O has started. Leave the committed lease fenced; normal
        // lease expiry recovers it, even if the process exits immediately.
        tracing::info!(
            stage = "response_spool_claim",
            outcome = "shutdown_after_commit",
            "response archive claim preserved for lease recovery"
        );
        return false;
    }
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

async fn observe_claim<T>(
    claim: impl Future<Output = Result<Option<T>, AppError>>,
) -> Result<Option<T>, AppError> {
    let started = tokio::time::Instant::now();
    // Two seconds is a diagnostic threshold, NOT a cancellation deadline.
    // A cold connection/query must be allowed to reach COMMIT/rollback.
    let result = claim.await;
    let elapsed = started.elapsed();
    let outcome = match &result {
        Ok(Some(_)) => "claimed",
        Ok(None) => "empty",
        Err(_) => "database_error",
    };
    if let Err(error) = &result {
        // SQLx separately emits a sanitized error_kind; never echo SQL/binds.
        tracing::warn!(
            stage = "response_spool_claim",
            outcome,
            elapsed_ms = elapsed.as_millis() as u64,
            error_category = error.diagnostic_category(),
            "response archive claim failed"
        );
    } else if elapsed >= SLOW_CLAIM {
        tracing::warn!(
            stage = "response_spool_claim",
            outcome,
            elapsed_ms = elapsed.as_millis() as u64,
            "response archive claim completed slowly"
        );
    }
    result
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
    let (lost_sender, mut lost_receiver) = tokio::sync::mpsc::channel(1);
    let heartbeat_state = state.clone();
    let heartbeat_task = task.clone();
    let mut heartbeat_attempt = attempt.clone();
    let _heartbeat = AbortOnDrop(tokio::spawn(async move {
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
                let _ = lost_sender.send(()).await;
                break;
            }
        }
    }));
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
        _ = lost_receiver.recv() => Err(AppError::Internal),
        result = transfer => result,
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test(start_paused = true)]
    async fn slow_claim_keeps_its_result_instead_of_cancelling() {
        let (entered, entering) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let claim = tokio::spawn(observe_claim(async {
            entered.send(()).unwrap();
            released.await.unwrap();
            Ok(Some(17))
        }));
        entering.await.unwrap();
        tokio::time::advance(SLOW_CLAIM * 3).await;
        assert!(!claim.is_finished(), "slow is not a cancellation deadline");
        release.send(()).unwrap();
        assert_eq!(claim.await.unwrap().unwrap(), Some(17));
    }

    #[tokio::test]
    async fn empty_claim_and_database_error_remain_distinct() {
        assert_eq!(
            observe_claim(async { Ok::<Option<()>, _>(None) })
                .await
                .unwrap(),
            None
        );
        assert!(matches!(
            observe_claim(async { Err::<Option<()>, _>(AppError::Internal) }).await,
            Err(AppError::Internal)
        ));
    }

    #[tokio::test]
    async fn shutdown_drains_current_boundary_without_starting_another_claim() {
        let (shutdown, receiver) = watch::channel(false);
        let (entered, mut entering) = tokio::sync::mpsc::channel(1);
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let drain_release = release.clone();
        let drain_completed = completed.clone();
        let drain = tokio::spawn(async move {
            drain_batch(&receiver, || async {
                entered.send(()).await.unwrap();
                let permit = drain_release.acquire().await.unwrap();
                permit.forget();
                drain_completed.fetch_add(1, Ordering::SeqCst);
                true
            })
            .await;
        });
        entering.recv().await.unwrap();
        shutdown.send(true).unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        release.add_permits(1);
        drain.await.unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        assert_eq!(entering.recv().await, None);
    }
}
