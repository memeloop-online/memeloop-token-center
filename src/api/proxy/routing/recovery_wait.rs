//! Bounded waiting for an unsent request; never permission to replay a POST.
use super::*;
use std::{
    future::Future,
    sync::{Arc, LazyLock},
    time::Duration,
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::Instant,
};

// A poll permit covers one check only and is released before the recheck sleep.
static POLLERS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(4)));
const RECHECK: Duration = Duration::from_millis(250);

#[cfg(test)]
pub(in crate::api::proxy) fn test_checkpoint(account: Uuid) -> Arc<tokio::sync::Notify> {
    static CHECKPOINTS: LazyLock<
        std::sync::Mutex<std::collections::HashMap<Uuid, std::sync::Weak<tokio::sync::Notify>>>,
    > = LazyLock::new(Default::default);
    let mut checkpoints = CHECKPOINTS.lock().unwrap();
    checkpoints.retain(|_, notify| notify.strong_count() > 0);
    if let Some(notify) = checkpoints.get(&account).and_then(std::sync::Weak::upgrade) {
        return notify;
    }
    let notify = Arc::new(tokio::sync::Notify::new());
    checkpoints.insert(account, Arc::downgrade(&notify));
    notify
}

enum Check<T> {
    Ready(T),
    Retry,
    Stop,
}

struct WaitObservation {
    request_id: Uuid,
    started: Instant,
    checks: u64,
    check_ms: u64,
    sleep_ms: u64,
    outcome: &'static str,
}

impl Drop for WaitObservation {
    fn drop(&mut self) {
        tracing::info!(
            request_id = %self.request_id,
            phase = "candidate_recovery_wait",
            outcome = self.outcome,
            elapsed_ms = self.started.elapsed().as_millis() as u64,
            checks = self.checks,
            check_ms = self.check_ms,
            sleep_ms = self.sleep_ms,
            "unsent candidate recovery wait summary"
        );
    }
}

#[cfg(test)]
async fn bounded_wait<T, F, Fut>(
    deadline: Instant,
    permits: Arc<Semaphore>,
    check: F,
) -> Result<Option<T>, AppError>
where
    F: FnMut(Arc<OwnedSemaphorePermit>) -> Fut,
    Fut: Future<Output = Result<Check<T>, AppError>>,
{
    bounded_wait_with_recheck(
        deadline,
        RECHECK,
        permits,
        crate::api::proxy_diagnostics::Context::current().request_id,
        check,
    )
    .await
}

async fn bounded_wait_with_recheck<T, F, Fut>(
    deadline: Instant,
    recheck: Duration,
    pollers: Arc<Semaphore>,
    request_id: Uuid,
    mut check: F,
) -> Result<Option<T>, AppError>
where
    F: FnMut(Arc<OwnedSemaphorePermit>) -> Fut,
    Fut: Future<Output = Result<Check<T>, AppError>>,
{
    let mut observation = WaitObservation {
        request_id,
        started: Instant::now(),
        checks: 0,
        check_ms: 0,
        sleep_ms: 0,
        outcome: "not_completed",
    };
    loop {
        if Instant::now() >= deadline {
            observation.outcome = "deadline";
            return Ok(None);
        }
        // DB queries/transactions finish before the timer; no DB transaction
        // spans a sleep. Cancellation drops a Ready value's attempt guard.
        let permit = match tokio::time::timeout_at(deadline, pollers.clone().acquire_owned()).await
        {
            Ok(Ok(permit)) => Arc::new(permit),
            Ok(Err(_)) => {
                observation.outcome = "capacity_closed";
                return Ok(None);
            }
            Err(_) => {
                observation.outcome = "deadline";
                return Ok(None);
            }
        };
        observation.checks += 1;
        let started = Instant::now();
        let checked = tokio::time::timeout_at(deadline, check(permit)).await;
        observation.check_ms += started.elapsed().as_millis() as u64;
        let checked = match checked {
            Ok(Ok(checked)) => checked,
            Ok(Err(error)) => {
                observation.outcome = "check_error";
                return Err(error);
            }
            Err(_) => {
                observation.outcome = "deadline";
                return Ok(None);
            }
        };
        match checked {
            Check::Ready(value) => {
                observation.outcome = "ready";
                return Ok(Some(value));
            }
            Check::Stop => {
                observation.outcome = "ineligible";
                return Ok(None);
            }
            Check::Retry => {
                let started = Instant::now();
                tokio::time::sleep_until((Instant::now() + recheck).min(deadline)).await;
                observation.sleep_ms += started.elapsed().as_millis() as u64;
            }
        }
    }
}

pub(crate) async fn wait(
    state: &AppState,
    request_id: Uuid,
    route: ResolvedUpstream,
    deadline: Instant,
) -> Result<
    Option<(
        ResolvedUpstream,
        UpstreamAttemptAdmission,
        UpstreamAttemptGuard,
    )>,
    AppError,
> {
    let policy = state.group_routing.as_ref().and_then(|snapshot| {
        snapshot
            .policy(
                route.route_id,
                route.account_id,
                route.credential_generation,
            )
            .map(|policy| (snapshot, policy))
    });
    let (deadline, recheck) = policy.map_or((deadline, RECHECK), |(snapshot, policy)| {
        policy.recovery_timing(snapshot, deadline, RECHECK)
    });
    bounded_wait_with_recheck(
        deadline,
        recheck,
        POLLERS.clone(),
        request_id,
        |permit| {
            let mut route = route.clone();
            let state = state.clone();
            // Owned checks finish lease publication/cleanup after caller cancellation;
            // the permit remains charged until that database work actually ends.
            let task = tokio::spawn(async move {
                let _permit = permit;
                if refresh_route_snapshot(&state, &mut route).await?
                    != PreparedRouteReadiness::Ready
                    || route
                        .credential
                        .expires_at()
                        .is_some_and(|expiry| expiry <= unix_millis())
                {
                    return Ok(Check::Stop);
                }
                let admission = if let Some(snapshot) = state.group_routing.as_ref()
                    && let Some((allow_probe, cooldown_ms)) = snapshot
                        .policy(
                            route.route_id,
                            route.account_id,
                            route.credential_generation,
                        )
                        .and_then(|policy| policy.transient_probe_controls())
                {
                    state
                        .db
                        .claim_upstream_account_attempt_at_revision_with_strategy(
                            snapshot.tenant_id,
                            route.account_id,
                            route.credential_generation,
                            route.transport_revision,
                            state.config.upstream_health,
                            allow_probe,
                            Some(cooldown_ms),
                            true,
                            Some(current_gateway_failure_domain()),
                        )
                        .await?
                } else {
                    state
                        .db
                        .claim_transient_recovery_attempt_at_revision(
                            route.account_id,
                            route.credential_generation,
                            route.transport_revision,
                            state.config.upstream_health,
                            Some(current_gateway_failure_domain()),
                        )
                        .await?
                };
                match admission {
                    UpstreamAttemptAdmission::Unavailable {
                        transient_wait_eligible: true,
                        ..
                    } => {
                        #[cfg(test)]
                        test_checkpoint(route.account_id).notify_one();
                        Ok(Check::Retry)
                    }
                    UpstreamAttemptAdmission::Unavailable { .. } => Ok(Check::Stop),
                    UpstreamAttemptAdmission::Healthy { .. }
                    | UpstreamAttemptAdmission::Probe { .. } => {
                        let guard = UpstreamAttemptGuard::new(
                            &state,
                            request_id,
                            route.route_id,
                            route.account_id,
                            route.credential_generation,
                            route.transport_revision,
                            admission,
                            None,
                        );
                        Ok(Check::Ready((route, admission, guard)))
                    }
                    UpstreamAttemptAdmission::SharedProbe { .. } => Ok(Check::Stop),
                }
            });
            async move { task.await.map_err(|_| AppError::Internal)? }
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Clone, Default)]
    struct LogWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn recovery_summary_separates_database_check_and_timer_without_poll_logs() {
        use tracing::instrument::WithSubscriber;
        let writer = LogWriter::default();
        let sink = writer.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_writer(move || sink.clone())
            .finish();
        let request_id = Uuid::new_v4();
        let mut checks = 0;
        let result = bounded_wait_with_recheck(
            Instant::now() + Duration::from_secs(5),
            RECHECK,
            Arc::new(Semaphore::new(1)),
            request_id,
            |_| {
                checks += 1;
                let ready = checks == 2;
                async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Ok(if ready { Check::Ready(7) } else { Check::Retry })
                }
            },
        )
        .with_subscriber(subscriber)
        .await
        .unwrap();
        assert_eq!(result, Some(7));
        let bytes = writer.0.lock().unwrap();
        let lines: Vec<_> = std::str::from_utf8(&bytes).unwrap().lines().collect();
        assert_eq!(lines.len(), 1);
        let event: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let fields = &event["fields"];
        assert_eq!(fields["request_id"], request_id.to_string());
        assert_eq!(fields["outcome"], "ready");
        assert_eq!(fields["checks"], 2);
        assert_eq!(fields["check_ms"], 200);
        assert_eq!(fields["sleep_ms"], 250);
        assert_eq!(fields["elapsed_ms"], 450);
    }
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn cancelled_owned_check_keeps_capacity_until_result_cleanup() {
        struct Published(Arc<tokio::sync::Notify>);
        impl Drop for Published {
            fn drop(&mut self) {
                self.0.notify_one();
            }
        }
        let permits = Arc::new(Semaphore::new(1));
        let semaphore = permits.clone();
        let (entered, observed) = tokio::sync::oneshot::channel();
        let (release, resume) = tokio::sync::oneshot::channel();
        let cleaned = Arc::new(tokio::sync::Notify::new());
        let cleanup = cleaned.clone();
        let caller = tokio::spawn(async move {
            let mut gate = Some((entered, resume, cleanup));
            bounded_wait(
                Instant::now() + Duration::from_secs(20),
                semaphore,
                |permit| {
                    let (entered, resume, cleanup) = gate.take().unwrap();
                    let task = tokio::spawn(async move {
                        let _permit = permit;
                        let _ = entered.send(());
                        resume.await.unwrap();
                        Ok(Check::Ready(Published(cleanup)))
                    });
                    async move { task.await.map_err(|_| AppError::Internal)? }
                },
            )
            .await
            .map(|_| ())
        });
        observed.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        assert_eq!(permits.available_permits(), 0);
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), cleaned.notified())
            .await
            .unwrap();
        assert_eq!(permits.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_and_capacity_bound_unsent_wait_without_replenishment() {
        let permits = Arc::new(Semaphore::new(1));
        let held = permits.clone().acquire_owned().await.unwrap();
        let calls = AtomicUsize::new(0);
        let deadline = Instant::now() + Duration::from_secs(1);
        let denied = bounded_wait::<(), _, _>(deadline, permits.clone(), |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(Check::Retry)
        })
        .await
        .unwrap();
        assert!(denied.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        drop(held);
        let retry_deadline = Instant::now() + Duration::from_secs(1);
        assert!(
            bounded_wait::<(), _, _>(retry_deadline, permits.clone(), |_| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Check::Retry)
            })
            .await
            .unwrap()
            .is_none()
        );
        assert_eq!(Instant::now(), retry_deadline);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(permits.available_permits(), 1);
    }

    #[tokio::test]
    async fn polling_capacity_queues_unsent_request_until_health_can_be_rechecked() {
        let permits = Arc::new(Semaphore::new(1));
        let held = permits.clone().acquire_owned().await.unwrap();
        let semaphore = permits.clone();
        let queued = tokio::spawn(async move {
            bounded_wait::<u8, _, _>(
                Instant::now() + Duration::from_secs(5),
                semaphore,
                |_| async { Ok(Check::Ready(9)) },
            )
            .await
            .unwrap()
        });
        tokio::task::yield_now().await;
        assert!(
            !queued.is_finished(),
            "capacity queues instead of rejecting"
        );
        drop(held);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), queued)
                .await
                .unwrap()
                .unwrap(),
            Some(9)
        );
        assert_eq!(permits.available_permits(), 1);
    }

    #[tokio::test]
    async fn sixty_five_waiters_queue_fairly_without_rejection_and_keep_four_db_checks() {
        let pollers = Arc::new(Semaphore::new(4));
        let held = pollers.clone().acquire_many_owned(4).await.unwrap();
        let release = Arc::new(Semaphore::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(Mutex::new(Vec::new()));
        let mut waiters = Vec::new();
        for request in 0..65 {
            let release = release.clone();
            let active = active.clone();
            let peak = peak.clone();
            let entered = entered.clone();
            let mut waiting = Box::pin(bounded_wait_with_recheck(
                Instant::now() + Duration::from_secs(20),
                RECHECK,
                pollers.clone(),
                Uuid::now_v7(),
                move |permit| {
                    let release = release.clone();
                    let active = active.clone();
                    let peak = peak.clone();
                    let entered = entered.clone();
                    async move {
                        let _permit = permit;
                        let concurrent = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(concurrent, Ordering::SeqCst);
                        entered.lock().unwrap().push(request);
                        release.acquire().await.unwrap().forget();
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok(Check::Ready(request))
                    }
                },
            ));
            std::future::poll_fn(|context| {
                assert!(waiting.as_mut().poll(context).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            waiters.push(waiting);
        }
        assert!(entered.lock().unwrap().is_empty());
        drop(held);
        for waiting in waiters.iter_mut().rev() {
            std::future::poll_fn(|context| {
                assert!(waiting.as_mut().poll(context).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
        }
        let mut first = entered.lock().unwrap().clone();
        first.sort_unstable();
        assert_eq!(first, vec![0, 1, 2, 3]);
        assert_eq!(active.load(Ordering::SeqCst), 4);
        assert_eq!(pollers.available_permits(), 0);
        release.add_permits(65);
        for (request, waiting) in waiters.into_iter().enumerate() {
            assert_eq!(waiting.await.unwrap(), Some(request));
        }
        assert_eq!(entered.lock().unwrap().len(), 65);
        assert_eq!(peak.load(Ordering::SeqCst), 4);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(pollers.available_permits(), 4);
    }

    #[tokio::test]
    async fn cancelling_queued_waiter_never_checks_or_blocks_the_next_waiter() {
        let pollers = Arc::new(Semaphore::new(1));
        let held = pollers.clone().acquire_owned().await.unwrap();
        let calls = AtomicUsize::new(0);
        let mut cancelled = Box::pin(bounded_wait(
            Instant::now() + Duration::from_secs(20),
            pollers.clone(),
            |_| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Check::Ready(()))
            },
        ));
        std::future::poll_fn(|context| {
            assert!(cancelled.as_mut().poll(context).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let mut next = Box::pin(bounded_wait(
            Instant::now() + Duration::from_secs(20),
            pollers.clone(),
            |_| async { Ok(Check::Ready(9)) },
        ));
        std::future::poll_fn(|context| {
            assert!(next.as_mut().poll(context).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(cancelled);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(pollers.available_permits(), 0);
        drop(held);
        assert_eq!(next.await.unwrap(), Some(9));
        assert_eq!(pollers.available_permits(), 1);
    }

    #[tokio::test]
    async fn cancellation_releases_poll_permit_and_preserves_closed_stop() {
        let permits = Arc::new(Semaphore::new(1));
        let (entered, observed) = tokio::sync::oneshot::channel();
        let semaphore = permits.clone();
        let task = tokio::spawn(async move {
            let mut entered = Some(entered);
            bounded_wait::<(), _, _>(
                Instant::now() + Duration::from_secs(20),
                semaphore,
                |permit| {
                    if let Some(entered) = entered.take() {
                        let _ = entered.send(());
                    }
                    async move {
                        let _permit = permit;
                        std::future::pending::<Result<Check<()>, AppError>>().await
                    }
                },
            )
            .await
        });
        observed.await.unwrap();
        assert_eq!(permits.available_permits(), 0);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(permits.available_permits(), 1);
        assert!(
            bounded_wait::<(), _, _>(
                Instant::now() + Duration::from_secs(20),
                permits,
                |_| async { Ok(Check::Stop) }
            )
            .await
            .unwrap()
            .is_none()
        );
    }
}
