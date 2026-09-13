use std::{future::Future, time::Duration};

use futures_util::{StreamExt, stream::FuturesUnordered};
use tokio::sync::watch;
use uuid::Uuid;

use crate::{
    AppState,
    db::ArchiveSpoolTask,
    error::AppError,
    proxy_lifecycle::{
        ProxyArchiveAttempt, begin_proxy_archive_attempt, heartbeat_proxy_archive_attempt,
    },
};

const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const SLOW_CLAIM: Duration = Duration::from_secs(2);
const UPLOAD_CONCURRENCY: usize = 4;
const CLAIMS_PER_DRAIN: usize = 32;

pub(crate) async fn run(state: AppState, shutdown: watch::Receiver<bool>) {
    let owner = Uuid::now_v7();
    let cleanup_state = state.clone();
    let drain_state = state;
    let drain_shutdown = shutdown.clone();
    run_scheduler(
        shutdown,
        move || {
            let state = cleanup_state.clone();
            async move { cleanup_spool_pass(&state).await }
        },
        move || {
            let state = drain_state.clone();
            let shutdown = drain_shutdown.clone();
            async move {
                drain_batch(&shutdown, || {
                    process_one_until_shutdown(&state, owner, Some(&shutdown))
                })
                .await;
            }
        },
    )
    .await;
}

async fn run_scheduler<C, Cleanup, D, Drain>(
    mut shutdown: watch::Receiver<bool>,
    cleanup: C,
    mut drain: D,
) where
    C: FnMut() -> Cleanup + Send + 'static,
    Cleanup: Future<Output = ()> + Send + 'static,
    D: FnMut() -> Drain + Send,
    Drain: Future<Output = ()> + Send,
{
    let cleanup_shutdown = shutdown.clone();
    let mut cleanup = tokio::spawn(run_periodic_cleanup(cleanup_shutdown, cleanup));
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    while !*shutdown.borrow() {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
            }
            result = &mut cleanup => {
                match result {
                    Ok(()) => tracing::error!(
                        stage = "response_spool_cleanup_task",
                        "response archive cleanup task stopped before shutdown"
                    ),
                    Err(_) => tracing::error!(
                        stage = "response_spool_cleanup_task",
                        "response archive cleanup task failed"
                    ),
                }
                return;
            }
            _ = interval.tick() => {
                drain().await;
            }
        }
    }
    if cleanup.await.is_err() {
        tracing::error!(
            stage = "response_spool_cleanup_task",
            "response archive cleanup task stopped"
        );
    }
}

async fn run_periodic_cleanup<F, Fut>(mut shutdown: watch::Receiver<bool>, mut cleanup: F)
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    while !*shutdown.borrow() {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
            }
            _ = interval.tick() => {
                // Finish each bounded pass before observing shutdown. The DB
                // helper commits one small batch at a time and checks its
                // wall-clock budget only between committed transactions.
                cleanup().await;
            }
        }
    }
}

async fn cleanup_spool_pass(state: &AppState) {
    // Keep each pass serial and bounded. Production schedules it independently
    // from uploads; the single-step test helper preserves its cleanup behavior.
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
}

async fn drain_batch<F, Fut>(shutdown: &watch::Receiver<bool>, mut process: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    // Each future also acquires the shared archive permit before claiming.
    // Refill free slots so one slow store operation does not stop the other
    // uploads. Never cancel a claimed transaction/upload on shutdown.
    let mut active = FuturesUnordered::new();
    let mut started = 0;
    while active.len() < UPLOAD_CONCURRENCY && !stopping(shutdown) {
        active.push(process());
        started += 1;
    }
    while let Some(progress) = active.next().await {
        if progress && started < CLAIMS_PER_DRAIN && !stopping(shutdown) {
            active.push(process());
            started += 1;
        }
    }
}

fn stopping(shutdown: &watch::Receiver<bool>) -> bool {
    *shutdown.borrow() || shutdown.has_changed().is_err()
}

#[cfg(test)]
pub(super) async fn process_one(state: &AppState, owner: Uuid) -> bool {
    // Preserve the single-step worker helper's cleanup semantics while the
    // production scheduler performs maintenance independently from claims.
    cleanup_spool_pass(state).await;
    process_one_until_shutdown(state, owner, None).await
}

async fn process_one_until_shutdown(
    state: &AppState,
    owner: Uuid,
    shutdown: Option<&watch::Receiver<bool>>,
) -> bool {
    process_one_with_admission(state, owner, shutdown, || !shutdown.is_some_and(stopping)).await
}

pub(super) async fn process_one_with_admission(
    state: &AppState,
    owner: Uuid,
    shutdown: Option<&watch::Receiver<bool>>,
    admit: impl FnOnce() -> bool,
) -> bool {
    // Stop at a committed transaction boundary instead of cancelling a live
    // SQL future. The latter can race SQLx's asynchronous rollback with the
    // next pooled BEGIN and generate transaction-state protocol notices.
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
    // Alternate queue priority across drain calls, so a permanently busy
    // response queue cannot starve requests (or vice versa).
    static REQUEST_FIRST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let request_first = REQUEST_FIRST.fetch_xor(true, std::sync::atomic::Ordering::Relaxed);
    let first = if request_first {
        super::BufferedArchivePurpose::Request
    } else {
        super::BufferedArchivePurpose::Response
    };
    let second = if request_first {
        super::BufferedArchivePurpose::Response
    } else {
        super::BufferedArchivePurpose::Request
    };
    let mut admit = Some(admit);
    let claimed = observe_claim(async {
        let first_task = {
            let allow = || admit.take().is_some_and(|check| check());
            state.db.claim_archive_spool_if(owner, first, allow).await?
        };
        match first_task {
            Some(task) => Ok(Some(task)),
            None if admit.is_some() => {
                state
                    .db
                    .claim_archive_spool_if(
                        owner,
                        second,
                        admit.take().expect("admission remains unused"),
                    )
                    .await
            }
            None => Ok(None),
        }
    })
    .await;
    let task = match claimed {
        Ok(Some(task)) => task,
        Ok(None) | Err(_) => return false,
    };
    // Admission happened inside the claim transaction. Complete this one
    // bounded attempt even if shutdown arrived during COMMIT; never consume a
    // retry merely to abandon a freshly committed claim without object I/O.
    let started = tokio::time::Instant::now();
    let mut phase = "staging_begin";
    let success = matches!(
        tokio::time::timeout(UPLOAD_TIMEOUT, upload(state, &task, &mut phase)).await,
        Ok(Ok(()))
    );
    if !success {
        let error_code = match phase {
            "decrypt" => "decrypt_failed",
            "lease" => "lease_lost",
            "chunk_validation" => "invalid_chunk",
            _ => "upload_failed",
        };
        // An upload/commit ACK may have been lost. Leave staged object cleanup
        // to the existing fenced reaper, which proves it unreferenced first.
        // retry() itself is fenced: a committed bind must never be undone.
        let retry_state = state.clone();
        let retry_task = task.clone();
        let _ = super::await_owned(
            Duration::from_secs(2),
            async move {
                retry_state
                    .db
                    .retry_response_archive_spool(&retry_task, error_code)
                    .await
            },
            "response_spool_retry",
        )
        .await;
        tracing::warn!(request_id = %task.identity.request_id, purpose = task.purpose.as_str(), phase, error_code, elapsed_ms = started.elapsed().as_millis() as u64, "durable archive retry pending");
    }
    true
}

pub(super) async fn observe_claim<T>(
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

async fn upload(
    state: &AppState,
    task: &ArchiveSpoolTask,
    phase: &mut &'static str,
) -> Result<(), AppError> {
    let _archive_memory = state.metrics.memory_usage(
        crate::metrics::MemoryComponent::ArchiveMultipart,
        crate::archive::ARCHIVE_MULTIPART_PART_BYTES + 1024 * 1024 + super::CHUNK_BYTES * 5,
    );
    let attempt_state = state.clone();
    let attempt_task = task.clone();
    let attempt = super::await_owned_unbounded(
        async move {
            begin_proxy_archive_attempt(
                &attempt_state.db,
                attempt_task.identity.request_id,
                attempt_task.purpose.staging(),
            )
            .await
        },
        "response_spool_staging_begin",
    )
    .await
    .ok_or(AppError::Internal)??;
    let (lost_sender, mut lost_receiver) = tokio::sync::mpsc::channel(1);
    let (heartbeat_stop, stop_receiver) = tokio::sync::oneshot::channel();
    let heartbeat_state = state.clone();
    let heartbeat_task = task.clone();
    let heartbeat_attempt = attempt.clone();
    let heartbeat = tokio::spawn(run_upload_heartbeat(
        heartbeat_state,
        heartbeat_task,
        heartbeat_attempt,
        stop_receiver,
        lost_sender,
    ));
    let transfer = async {
        *phase = "object_start";
        let mut writer = state.archive.start_writer(&attempt.object_locator).await?;
        let mut total = 0_i64;
        let mut seq = 0;
        while seq < task.chunk_count {
            *phase = "chunk_load";
            let chunks = state
                .db
                .load_response_archive_spool_batch(task, seq)
                .await?;
            if chunks.is_empty() {
                *phase = "lease";
                return Err(AppError::Internal);
            }
            for chunk in chunks {
                *phase = "chunk_validation";
                if chunk.seq != seq || seq >= task.chunk_count {
                    return Err(AppError::Internal);
                }
                *phase = "decrypt";
                let bytes = super::cipher::open_for_purpose(
                    task.identity,
                    seq,
                    &chunk.ciphertext,
                    chunk.byte_count,
                    state.config.key_pepper.as_bytes(),
                    task.purpose,
                )?;
                total = total
                    .checked_add(chunk.byte_count)
                    .ok_or(AppError::Internal)?;
                *phase = "object_write";
                writer.write(bytes).await?;
                seq += 1;
            }
        }
        *phase = "chunk_validation";
        if total != task.byte_count {
            return Err(AppError::Internal);
        }
        *phase = "object_finish";
        let stored = writer.finish_staged().await?;
        if stored.object_locator != attempt.object_locator
            || stored.size_bytes != u64::try_from(total).map_err(|_| AppError::Internal)?
        {
            return Err(AppError::Internal);
        }
        *phase = "terminal_bind";
        let bind_state = state.clone();
        let bind_task = task.clone();
        let bind_lease = attempt.lease.clone();
        let bind_locator = stored.object_locator.clone();
        let bound = super::await_owned_unbounded(
            async move {
                bind_state
                    .db
                    .complete_response_archive_spool(&bind_task, &bind_lease, &bind_locator)
                    .await
            },
            "response_spool_terminal_bind",
        )
        .await
        .ok_or(AppError::Internal)??;
        if !bound {
            return Err(AppError::Internal);
        }
        Ok(())
    };
    let outcome = tokio::select! {
        biased;
        _ = lost_receiver.recv() => None,
        result = transfer => Some(result),
    };
    let _ = heartbeat_stop.send(());
    if let Err(error) = heartbeat.await {
        tracing::error!(
            stage = "response_spool_heartbeat_task",
            task_cancelled = error.is_cancelled(),
            task_panicked = error.is_panic(),
            "response archive heartbeat task failed"
        );
    }
    match outcome {
        Some(result) => result,
        None => {
            *phase = "lease";
            Err(AppError::Internal)
        }
    }
}

async fn run_upload_heartbeat(
    state: AppState,
    task: ArchiveSpoolTask,
    attempt: ProxyArchiveAttempt,
    stop: tokio::sync::oneshot::Receiver<()>,
    lost: tokio::sync::mpsc::Sender<()>,
) {
    run_heartbeat_loop(attempt, stop, lost, move |mut attempt| {
        let heartbeat_state = state.clone();
        let heartbeat_task = task.clone();
        async move {
            match super::await_owned(
                Duration::from_secs(2),
                async move {
                    let live = heartbeat_state
                        .db
                        .heartbeat_response_archive_spool(&heartbeat_task)
                        .await?
                        && heartbeat_proxy_archive_attempt(&heartbeat_state.db, &mut attempt)
                            .await?;
                    Ok::<_, AppError>((live, attempt))
                },
                "response_spool_heartbeat",
            )
            .await
            {
                Some(Ok((true, attempt))) => Some(attempt),
                _ => None,
            }
        }
    })
    .await;
}

async fn run_heartbeat_loop<A, R, Renewal>(
    mut attempt: A,
    mut stop: tokio::sync::oneshot::Receiver<()>,
    lost: tokio::sync::mpsc::Sender<()>,
    mut renew: R,
) where
    R: FnMut(A) -> Renewal,
    Renewal: Future<Output = Option<A>>,
{
    loop {
        tokio::select! {
            biased;
            _ = &mut stop => break,
            _ = tokio::time::sleep(Duration::from_secs(10)) => {}
        }
        let Some(renewed) = renew(attempt).await else {
            let _ = lost.send(()).await;
            break;
        };
        attempt = renewed;
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

    #[tokio::test(start_paused = true)]
    async fn heartbeat_stop_waits_for_the_current_renewal_boundary() {
        let (stop, stop_receiver) = tokio::sync::oneshot::channel();
        let (lost, mut lost_receiver) = tokio::sync::mpsc::channel(1);
        let (entered, entering) = tokio::sync::oneshot::channel();
        let mut entered = Some(entered);
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let renewal_release = release.clone();
        let renewals = Arc::new(AtomicUsize::new(0));
        let renewal_count = renewals.clone();
        let heartbeat = tokio::spawn(run_heartbeat_loop((), stop_receiver, lost, move |()| {
            let entered = entered.take();
            let release = renewal_release.clone();
            renewal_count.fetch_add(1, Ordering::SeqCst);
            async move {
                if let Some(entered) = entered {
                    entered.send(()).unwrap();
                }
                release.acquire().await.unwrap().forget();
                Some(())
            }
        }));
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::time::timeout(Duration::from_secs(1), entering)
            .await
            .expect("heartbeat renewal must start")
            .unwrap();
        stop.send(()).unwrap();
        tokio::task::yield_now().await;
        assert!(
            !heartbeat.is_finished(),
            "stopping must not cancel a renewal that can own a transaction"
        );
        release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(1), heartbeat)
            .await
            .expect("heartbeat must stop after the active renewal settles")
            .unwrap();
        assert_eq!(renewals.load(Ordering::SeqCst), 1);
        assert!(lost_receiver.try_recv().is_err());
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
        let (entered, mut entering) = tokio::sync::mpsc::channel(UPLOAD_CONCURRENCY);
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
        for _ in 0..UPLOAD_CONCURRENCY {
            entering.recv().await.unwrap();
        }
        shutdown.send(true).unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        release.add_permits(UPLOAD_CONCURRENCY);
        drain.await.unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), UPLOAD_CONCURRENCY);
        assert_eq!(entering.recv().await, None);
    }

    #[tokio::test]
    async fn bounded_drain_refills_around_a_slow_upload_and_stops_at_32() {
        let (_shutdown, receiver) = watch::channel(false);
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let (entered, mut entering) = tokio::sync::mpsc::unbounded_channel();
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let drain_release = release.clone();
        let drain_peak = peak.clone();
        let drain_calls = calls.clone();
        let drain = tokio::spawn(async move {
            drain_batch(&receiver, || {
                let index = drain_calls.fetch_add(1, Ordering::SeqCst);
                let release = drain_release.clone();
                let running = running.clone();
                let peak = drain_peak.clone();
                let entered = entered.clone();
                async move {
                    let count = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(count, Ordering::SeqCst);
                    entered.send(index).unwrap();
                    if index < UPLOAD_CONCURRENCY {
                        release.acquire().await.unwrap().forget();
                    }
                    running.fetch_sub(1, Ordering::SeqCst);
                    true
                }
            })
            .await;
        });
        let mut first = Vec::new();
        for _ in 0..UPLOAD_CONCURRENCY {
            first.push(entering.recv().await.unwrap());
        }
        first.sort_unstable();
        assert_eq!(first, (0..UPLOAD_CONCURRENCY).collect::<Vec<_>>());
        assert_eq!(peak.load(Ordering::SeqCst), UPLOAD_CONCURRENCY);
        // Three blocked uploads remain; the released slot drains the queue.
        release.add_permits(1);
        for expected in UPLOAD_CONCURRENCY..CLAIMS_PER_DRAIN {
            assert_eq!(entering.recv().await.unwrap(), expected);
        }
        assert!(!drain.is_finished());
        release.add_permits(UPLOAD_CONCURRENCY - 1);
        drain.await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), CLAIMS_PER_DRAIN);
        assert_eq!(peak.load(Ordering::SeqCst), UPLOAD_CONCURRENCY);
    }

    #[tokio::test(start_paused = true)]
    async fn cleanup_keeps_running_while_a_drain_waits_for_slow_uploads() {
        let (shutdown, receiver) = watch::channel(false);
        let cleanup_release = Arc::new(tokio::sync::Semaphore::new(0));
        let pass_release = cleanup_release.clone();
        let (pass_entered, mut pass_entering) = tokio::sync::mpsc::unbounded_channel();
        let (pass_completed, mut pass_completing) = tokio::sync::mpsc::unbounded_channel();
        let cleanup_calls = Arc::new(AtomicUsize::new(0));
        let cleanup_counter = cleanup_calls.clone();
        let drain_release = Arc::new(tokio::sync::Semaphore::new(0));
        let release_drain = drain_release.clone();
        let (drain_entered, drain_entering) = tokio::sync::oneshot::channel();
        let mut drain_entered = Some(drain_entered);
        let scheduler = tokio::spawn(run_scheduler(
            receiver,
            move || {
                let calls = cleanup_counter.clone();
                let release = pass_release.clone();
                let entered = pass_entered.clone();
                let completed = pass_completed.clone();
                async move {
                    let index = calls.fetch_add(1, Ordering::SeqCst);
                    entered.send(index).unwrap();
                    if index == 3 {
                        release.acquire().await.unwrap().forget();
                    }
                    completed.send(index).unwrap();
                }
            },
            move || {
                let release = release_drain.clone();
                let entered = drain_entered.take();
                async move {
                    if let Some(entered) = entered {
                        entered.send(()).unwrap();
                    }
                    release.acquire().await.unwrap().forget();
                }
            },
        ));
        tokio::time::timeout(Duration::from_secs(1), drain_entering)
            .await
            .expect("scheduler must start its upload drain")
            .unwrap();

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), pass_entering.recv())
                .await
                .expect("first cleanup pass must start")
                .unwrap(),
            0
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), pass_completing.recv())
                .await
                .expect("first cleanup pass must complete")
                .unwrap(),
            0
        );
        for expected in 1..=2 {
            tokio::time::advance(Duration::from_secs(1)).await;
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), pass_entering.recv())
                    .await
                    .expect("periodic cleanup pass must start")
                    .unwrap(),
                expected
            );
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), pass_completing.recv())
                    .await
                    .expect("periodic cleanup pass must complete")
                    .unwrap(),
                expected
            );
        }
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), pass_entering.recv())
                .await
                .expect("cleanup must remain scheduled during the slow drain")
                .unwrap(),
            3
        );
        assert_eq!(cleanup_calls.load(Ordering::SeqCst), 4);

        shutdown.send(true).unwrap();
        tokio::task::yield_now().await;
        assert!(
            !scheduler.is_finished(),
            "shutdown must not cancel an in-flight cleanup pass"
        );
        cleanup_release.add_permits(1);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), pass_completing.recv())
                .await
                .expect("shutdown must wait for the active cleanup pass")
                .unwrap(),
            3
        );
        assert!(
            !scheduler.is_finished(),
            "the scheduler must also drain its current upload boundary"
        );
        drain_release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(1), scheduler)
            .await
            .expect("scheduler must finish after both active boundaries")
            .unwrap();
    }
}
