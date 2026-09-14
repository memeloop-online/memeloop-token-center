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

// The request retains its existing byte reservation while waiting. This adds
// a separate, fail-fast count bound; it never allocates or copies body bytes.
static WAITERS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(4)));
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

#[cfg(test)]
async fn bounded_wait<T, F, Fut>(
    deadline: Instant,
    permits: Arc<Semaphore>,
    metrics: Option<&crate::metrics::Metrics>,
    check: F,
) -> Result<Option<T>, AppError>
where
    F: FnMut(Arc<OwnedSemaphorePermit>) -> Fut,
    Fut: Future<Output = Result<Check<T>, AppError>>,
{
    bounded_wait_with_recheck(deadline, RECHECK, permits, metrics, check).await
}

async fn bounded_wait_with_recheck<T, F, Fut>(
    deadline: Instant,
    recheck: Duration,
    permits: Arc<Semaphore>,
    metrics: Option<&crate::metrics::Metrics>,
    mut check: F,
) -> Result<Option<T>, AppError>
where
    F: FnMut(Arc<OwnedSemaphorePermit>) -> Fut,
    Fut: Future<Output = Result<Check<T>, AppError>>,
{
    let Ok(permit) = permits.try_acquire_owned() else {
        if let Some(metrics) = metrics {
            metrics.observe_upstream_health(
                UpstreamHealthEvent::Skipped,
                UpstreamHealthReason::RecoveryWaitCapacity,
            );
        }
        return Ok(None);
    };
    let permit = Arc::new(permit);
    loop {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        // DB queries/transactions finish before the timer; no DB transaction
        // spans a sleep. Cancellation drops a Ready value's attempt guard.
        let checked = match tokio::time::timeout_at(deadline, check(permit.clone())).await {
            Ok(checked) => checked?,
            Err(_) => return Ok(None),
        };
        match checked {
            Check::Ready(value) => return Ok(Some(value)),
            Check::Stop => return Ok(None),
            Check::Retry => {
                tokio::time::sleep_until((Instant::now() + recheck).min(deadline)).await
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
        (policy.wait_deadline(snapshot, deadline), policy.recheck())
    });
    bounded_wait_with_recheck(
        deadline,
        recheck,
        WAITERS.clone(),
        Some(&state.metrics),
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
                    && let Some(policy) = snapshot.policy(
                        route.route_id,
                        route.account_id,
                        route.credential_generation,
                    ) {
                    state
                        .db
                        .claim_upstream_account_attempt_with_strategy(
                            snapshot.tenant_id,
                            route.account_id,
                            route.credential_generation,
                            state.config.upstream_health,
                            policy.allow_probe(),
                            Some(policy.cooldown_ms()),
                            true,
                        )
                        .await?
                } else {
                    state
                        .db
                        .claim_transient_recovery_attempt(
                            route.account_id,
                            route.credential_generation,
                            state.config.upstream_health,
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
                    UpstreamAttemptAdmission::Healthy | UpstreamAttemptAdmission::Probe { .. } => {
                        let guard = UpstreamAttemptGuard::new(
                            &state,
                            request_id,
                            route.route_id,
                            route.account_id,
                            route.credential_generation,
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
                None,
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
        let denied = bounded_wait::<(), _, _>(deadline, permits.clone(), None, |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(Check::Retry)
        })
        .await
        .unwrap();
        assert!(denied.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        drop(held);
        assert!(
            bounded_wait::<(), _, _>(deadline, permits.clone(), None, |_| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Check::Retry)
            })
            .await
            .unwrap()
            .is_none()
        );
        assert_eq!(Instant::now(), deadline);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(permits.available_permits(), 1);
    }

    #[tokio::test]
    async fn cancellation_releases_waiter_and_preserves_closed_stop() {
        let permits = Arc::new(Semaphore::new(1));
        let (entered, observed) = tokio::sync::oneshot::channel();
        let semaphore = permits.clone();
        let task = tokio::spawn(async move {
            let mut entered = Some(entered);
            bounded_wait::<(), _, _>(
                Instant::now() + Duration::from_secs(20),
                semaphore,
                None,
                |_| {
                    if let Some(entered) = entered.take() {
                        let _ = entered.send(());
                    }
                    std::future::pending::<Result<Check<()>, AppError>>()
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
                None,
                |_| async { Ok(Check::Stop) }
            )
            .await
            .unwrap()
            .is_none()
        );
    }
}
