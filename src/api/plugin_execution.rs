//! Host-owned diagnostics for bounded component invocation, not delivery or retry authority.
use std::{
    sync::{Arc, LazyLock},
    time::Duration,
};

use tokio::{sync::Semaphore, time::Instant};
use uuid::Uuid;

use crate::error::AppError;

static PERMITS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(8)));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    PostAuth,
    Prepare,
    Normalize,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Self::PostAuth => "post_auth",
            Self::Prepare => "prepare",
            Self::Normalize => "normalize",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Returned,
    HookError,
    CapacityTimeout,
    CapacityClosed,
    ExecutionTimeout,
    TaskFailed,
    CallerCancelled,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Returned => "returned",
            Self::HookError => "hook_error",
            Self::CapacityTimeout => "capacity_timeout",
            Self::CapacityClosed => "capacity_closed",
            Self::ExecutionTimeout => "execution_timeout",
            Self::TaskFailed => "task_failed",
            Self::CallerCancelled => "caller_cancelled",
        }
    }
}

struct Event {
    invocation_id: Uuid,
    phase: Phase,
    outcome: Outcome,
    elapsed: Duration,
}

struct Observation<O: FnOnce(Event)> {
    invocation_id: Uuid,
    phase: Phase,
    started: Instant,
    outcome: Outcome,
    report: Option<O>,
}

impl<O: FnOnce(Event)> Drop for Observation<O> {
    fn drop(&mut self) {
        if let Some(report) = self.report.take() {
            report(Event {
                invocation_id: self.invocation_id,
                phase: self.phase,
                outcome: self.outcome,
                elapsed: self.started.elapsed(),
            });
        }
    }
}

fn emit_event(event: Event) {
    tracing::info!(
        event = "plugin_execution_observed",
        invocation_id = %event.invocation_id,
        phase = event.phase.as_str(),
        outcome = event.outcome.as_str(),
        elapsed_ms = event.elapsed.as_millis() as u64,
        "host completed waiting for component invocation"
    );
}

pub(super) async fn run<T, F>(phase: Phase, work: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
{
    run_with_limits(
        phase,
        work,
        PERMITS.clone(),
        Duration::from_secs(1),
        Duration::from_secs(35),
        emit_event,
    )
    .await
}

async fn run_with_limits<T, F, O>(
    phase: Phase,
    work: F,
    permits: Arc<Semaphore>,
    capacity_timeout: Duration,
    execution_timeout: Duration,
    report: O,
) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
    O: FnOnce(Event),
{
    let mut observation = Observation {
        invocation_id: Uuid::new_v4(),
        phase,
        started: Instant::now(),
        outcome: Outcome::CallerCancelled,
        report: Some(report),
    };
    let permit = match tokio::time::timeout(capacity_timeout, permits.acquire_owned()).await {
        Err(_) => {
            observation.outcome = Outcome::CapacityTimeout;
            return Err(AppError::Upstream(
                "plugin execution capacity is exhausted".into(),
            ));
        }
        Ok(Err(_)) => {
            observation.outcome = Outcome::CapacityClosed;
            return Err(AppError::Internal);
        }
        Ok(Ok(permit)) => permit,
    };
    let span = tracing::info_span!("plugin_invocation", invocation_id = %observation.invocation_id, phase = phase.as_str());
    let task = tokio::task::spawn_blocking(move || {
        // A timed-out/cancelled caller cannot release capacity still occupied
        // by blocking Wasm. The runtime's own fuel/epoch limits remain in force.
        let _permit = permit;
        span.in_scope(work)
    });
    match tokio::time::timeout(execution_timeout, task).await {
        Err(_) => {
            observation.outcome = Outcome::ExecutionTimeout;
            Err(AppError::Upstream("plugin execution timed out".into()))
        }
        Ok(Err(_)) => {
            observation.outcome = Outcome::TaskFailed;
            Err(AppError::Upstream("plugin task failed".into()))
        }
        Ok(Ok(Err(error))) => {
            observation.outcome = Outcome::HookError;
            Err(error)
        }
        Ok(Ok(Ok(value))) => {
            observation.outcome = Outcome::Returned;
            Ok(value)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn hook_error_keeps_control_flow_but_reports_only_a_closed_outcome() {
        let (send, receive) = oneshot::channel();
        let result = run_with_limits::<(), _, _>(
            Phase::Prepare,
            || Err(AppError::Upstream("private-fixture-payload".into())),
            Arc::new(Semaphore::new(1)),
            Duration::from_secs(1),
            Duration::from_secs(35),
            move |event| {
                let _ = send.send(event);
            },
        )
        .await;
        assert!(matches!(result, Err(AppError::Upstream(_))));
        let event = receive.await.unwrap();
        assert_eq!(event.phase, Phase::Prepare);
        assert_eq!(event.outcome, Outcome::HookError);
        assert!(!event.invocation_id.is_nil());
    }

    #[tokio::test(start_paused = true)]
    async fn capacity_timeout_never_executes_the_hook() {
        let (send, receive) = oneshot::channel();
        let result = run_with_limits(
            Phase::PostAuth,
            || -> Result<(), AppError> { panic!("capacity denied hook must not execute") },
            Arc::new(Semaphore::new(0)),
            Duration::from_secs(1),
            Duration::from_secs(35),
            move |event| {
                let _ = send.send(event);
            },
        )
        .await;
        assert!(matches!(result, Err(AppError::Upstream(_))));
        assert_eq!(receive.await.unwrap().outcome, Outcome::CapacityTimeout);
    }

    #[tokio::test]
    async fn cancelled_caller_retains_capacity_until_blocking_hook_exits() {
        let permits = Arc::new(Semaphore::new(1));
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (event_tx, event_rx) = oneshot::channel();
        let task = tokio::spawn(run_with_limits(
            Phase::Normalize,
            move || {
                let _ = started_tx.send(());
                release_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("test releases blocked hook");
                Ok(())
            },
            permits.clone(),
            Duration::from_secs(1),
            Duration::from_secs(35),
            move |event| {
                let _ = event_tx.send(event);
            },
        ));
        started_rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(event_rx.await.unwrap().outcome, Outcome::CallerCancelled);
        assert_eq!(permits.available_permits(), 0);
        release_tx.send(()).unwrap();
        let permit = tokio::time::timeout(Duration::from_secs(5), permits.acquire())
            .await
            .unwrap()
            .unwrap();
        drop(permit);
        assert_eq!(permits.available_permits(), 1);
    }

    #[tokio::test]
    async fn normal_return_releases_capacity_and_reports_returned_not_delivery_success() {
        let permits = Arc::new(Semaphore::new(1));
        let (send, receive) = oneshot::channel();
        let result = run_with_limits(
            Phase::PostAuth,
            || Ok(false),
            permits.clone(),
            Duration::from_secs(1),
            Duration::from_secs(35),
            move |event| {
                let _ = send.send(event);
            },
        )
        .await
        .unwrap();
        assert!(
            !result,
            "a policy denial is still a successfully returned hook result"
        );
        assert_eq!(receive.await.unwrap().outcome, Outcome::Returned);
        assert_eq!(permits.available_permits(), 1);
    }

    #[tokio::test]
    async fn execution_timeout_does_not_free_capacity_owned_by_a_blocked_hook() {
        let permits = Arc::new(Semaphore::new(1));
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (event_tx, event_rx) = oneshot::channel();
        let result = run_with_limits(
            Phase::Prepare,
            move || {
                let _ = started_tx.send(());
                release_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("test releases blocked hook");
                Ok(())
            },
            permits.clone(),
            Duration::from_secs(1),
            Duration::ZERO,
            move |event| {
                let _ = event_tx.send(event);
            },
        )
        .await;
        assert!(matches!(result, Err(AppError::Upstream(_))));
        assert_eq!(event_rx.await.unwrap().outcome, Outcome::ExecutionTimeout);
        // The worker may begin before or after the zero-duration timeout;
        // neither ordering is assumed. It cannot finish before this release.
        tokio::time::timeout(Duration::from_secs(5), started_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(permits.available_permits(), 0);
        release_tx.send(()).unwrap();
        let permit = tokio::time::timeout(Duration::from_secs(5), permits.acquire())
            .await
            .unwrap()
            .unwrap();
        drop(permit);
        assert_eq!(permits.available_permits(), 1);
    }

    #[tokio::test]
    async fn closed_capacity_is_distinct_from_wait_timeout() {
        let permits = Arc::new(Semaphore::new(1));
        permits.close();
        let (send, receive) = oneshot::channel();
        let result = run_with_limits(
            Phase::PostAuth,
            || -> Result<(), AppError> { panic!("closed capacity must not execute") },
            permits,
            Duration::from_secs(1),
            Duration::from_secs(35),
            move |event| {
                let _ = send.send(event);
            },
        )
        .await;
        assert!(matches!(result, Err(AppError::Internal)));
        assert_eq!(receive.await.unwrap().outcome, Outcome::CapacityClosed);
    }
}
