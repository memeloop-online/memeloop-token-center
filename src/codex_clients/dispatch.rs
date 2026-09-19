//! Account/endpoint FIFO admission, independent of HTTP client cache revisions.
use crate::{
    metrics::Metrics,
    provider::{CodexTransportPolicy, ResolvedUpstream},
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};

#[derive(Clone, Hash, Eq, PartialEq)]
struct LaneKey {
    account_id: uuid::Uuid,
    endpoint: [u8; 32],
}

impl LaneKey {
    fn new(route: &ResolvedUpstream) -> Self {
        // Never hash credentials, query strings or paths into the lane identity.
        // Rotating proxy authentication must not multiply an endpoint's budget.
        let endpoint = route
            .credential
            .proxy()
            .and_then(|(raw, _)| url::Url::parse(raw).ok())
            .map(|url| {
                format!(
                    "{}://{}:{}",
                    url.scheme(),
                    url.host_str().unwrap_or(""),
                    url.port_or_known_default().unwrap_or(1080)
                )
            })
            .unwrap_or_else(|| "direct".to_owned());
        Self {
            account_id: route.account_id,
            endpoint: Sha256::digest(endpoint.as_bytes()).into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DispatchError {
    Capacity,
    Timeout,
    InvalidPolicy,
}

impl DispatchError {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Capacity => "codex_dispatch_queue_capacity",
            Self::Timeout => "codex_dispatch_queue_timeout",
            Self::InvalidPolicy => "invalid_transport_policy",
        }
    }

    pub(crate) fn response(self) -> axum::response::Response {
        use axum::response::IntoResponse;
        (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            [(axum::http::header::RETRY_AFTER, "1")],
            axum::Json(serde_json::json!({"error": {
                "type": "service_overloaded", "code": self.code(),
                "message": "Codex dispatch capacity is temporarily unavailable"
            }})),
        )
            .into_response()
    }
}

struct State {
    revision: i64,
    maximum: usize,
    max_queued: usize,
    timeout: Duration,
    active: usize,
    queued: usize,
}
struct Lane {
    key: LaneKey,
    state: Mutex<State>,
    turn: Arc<Semaphore>,
    changed: Notify,
}

impl Lane {
    fn configure(&self, revision: i64, policy: CodexTransportPolicy) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if revision < state.revision {
            return;
        }
        state.revision = revision;
        state.maximum = policy.dispatch_max_in_flight;
        state.max_queued = policy.dispatch_max_queued;
        state.timeout = Duration::from_millis(policy.dispatch_queue_timeout_millis);
        self.changed.notify_one();
    }

    fn take(self: &Arc<Self>, metrics: &Metrics) -> Option<DispatchPermit> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active >= state.maximum {
            return None;
        }
        state.active += 1;
        Some(DispatchPermit {
            lane: self.clone(),
            metrics: metrics.clone(),
        })
    }

    async fn acquire(self: &Arc<Self>, metrics: &Metrics) -> Result<DispatchPermit, DispatchError> {
        // Tokio's fair semaphore is a turnstile, not the dynamic capacity.
        // Only its FIFO head waits for capacity, so shrinking cannot leak
        // permits through cancellation or grant new work above the new limit.
        if let Ok(turn) = self.turn.clone().try_acquire_owned()
            && let Some(permit) = self.take(metrics)
        {
            drop(turn);
            metrics.observe_codex_dispatch("admitted");
            return Ok(permit);
        }
        let timeout = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.queued >= state.max_queued {
                metrics.observe_codex_dispatch("capacity");
                return Err(DispatchError::Capacity);
            }
            state.queued += 1;
            state.timeout
        };
        let mut waiter = Waiter {
            lane: self.clone(),
            metrics: metrics.clone(),
            finished: false,
        };
        metrics.observe_codex_dispatch("queued");
        let result = tokio::time::timeout(timeout, async {
            let _turn = self
                .turn
                .clone()
                .acquire_owned()
                .await
                .expect("lane never closed");
            loop {
                let notified = self.changed.notified();
                if let Some(permit) = self.take(metrics) {
                    return permit;
                }
                notified.await;
            }
        })
        .await
        .map_err(|_| DispatchError::Timeout);
        waiter.finished = true;
        metrics.observe_codex_dispatch(if result.is_ok() {
            "admitted"
        } else {
            "timeout"
        });
        result
    }
}

struct Waiter {
    lane: Arc<Lane>,
    metrics: Metrics,
    finished: bool,
}
impl Drop for Waiter {
    fn drop(&mut self) {
        self.lane
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .queued -= 1;
        if !self.finished {
            self.metrics.observe_codex_dispatch("cancelled");
        }
    }
}

pub(crate) struct DispatchPermit {
    lane: Arc<Lane>,
    metrics: Metrics,
}
impl DispatchPermit {
    pub(crate) fn matches(&self, route: &ResolvedUpstream) -> bool {
        self.lane.key == LaneKey::new(route)
    }
}
impl Drop for DispatchPermit {
    fn drop(&mut self) {
        self.lane
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active -= 1;
        self.lane.changed.notify_one();
        self.metrics.observe_codex_dispatch("released");
    }
}

#[derive(Default)]
pub(super) struct DispatchLanes {
    lanes: Mutex<HashMap<LaneKey, Weak<Lane>>>,
}
impl DispatchLanes {
    pub(super) async fn acquire(
        &self,
        route: &ResolvedUpstream,
        metrics: &Metrics,
    ) -> Result<DispatchPermit, DispatchError> {
        let policy = CodexTransportPolicy::parse(route.config.get("transport_policy"))
            .map_err(|_| DispatchError::InvalidPolicy)?;
        let key = LaneKey::new(route);
        let lane = {
            let mut lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
            lanes.retain(|_, lane| lane.strong_count() > 0);
            lanes.get(&key).and_then(Weak::upgrade).unwrap_or_else(|| {
                let lane = Arc::new(Lane {
                    key: key.clone(),
                    state: Mutex::new(State {
                        revision: route.transport_revision,
                        maximum: policy.dispatch_max_in_flight,
                        max_queued: policy.dispatch_max_queued,
                        timeout: Duration::from_millis(policy.dispatch_queue_timeout_millis),
                        active: 0,
                        queued: 0,
                    }),
                    turn: Arc::new(Semaphore::new(1)),
                    changed: Notify::new(),
                });
                lanes.insert(key, Arc::downgrade(&lane));
                lane
            })
        };
        lane.configure(route.transport_revision, policy);
        let started = std::time::Instant::now();
        let result = lane.acquire(metrics).await;
        tracing::info!(upstream_account_id = %route.account_id, stage = "codex_dispatch_admission", outcome = result.as_ref().err().map_or("admitted", |e| e.code()), wait_millis = started.elapsed().as_millis() as u64, "Codex dispatch admission completed");
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::UpstreamCredential;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn route() -> ResolvedUpstream {
        ResolvedUpstream {
            route_id: uuid::Uuid::nil(),
            account_id: uuid::Uuid::nil(),
            transport_revision: 1,
            credential_generation: 1,
            driver: "openai-codex".into(),
            base_url: String::new(),
            config: serde_json::json!({"transport_policy": {"dispatch_max_in_flight": 2, "dispatch_max_queued": 32, "dispatch_queue_timeout_millis": 50}}),
            upstream_model: String::new(),
            credential: UpstreamCredential::None,
        }
    }

    #[tokio::test]
    async fn twelve_concurrent_requests_have_bounded_peak_and_release_all_lanes() {
        let lanes = Arc::new(DispatchLanes::default());
        let barrier = Arc::new(tokio::sync::Barrier::new(12));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..12 {
            let (lanes, barrier, active, peak) =
                (lanes.clone(), barrier.clone(), active.clone(), peak.clone());
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                let mut route = route();
                route.config["transport_policy"]["dispatch_queue_timeout_millis"] =
                    serde_json::json!(5000);
                let _permit = lanes.acquire(&route, &Metrics::default()).await.unwrap();
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(count, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
                active.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert!(
            lanes
                .lanes
                .lock()
                .unwrap()
                .values()
                .all(|lane| lane.strong_count() == 0)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_capacity_and_cancellation_release_fifo_waiters() {
        let lanes = Arc::new(DispatchLanes::default());
        let mut route = route();
        route.config["transport_policy"]["dispatch_max_in_flight"] = serde_json::json!(1);
        route.config["transport_policy"]["dispatch_max_queued"] = serde_json::json!(1);
        let metrics = Metrics::default();
        let held = lanes.acquire(&route, &metrics).await.unwrap();
        let waiting = {
            let (lanes, route, metrics) = (lanes.clone(), route.clone(), metrics.clone());
            tokio::spawn(async move { lanes.acquire(&route, &metrics).await })
        };
        tokio::task::yield_now().await;
        assert!(matches!(
            lanes.acquire(&route, &metrics).await,
            Err(DispatchError::Capacity)
        ));
        waiting.abort();
        assert!(waiting.await.is_err());
        assert_eq!(held.lane.state.lock().unwrap().queued, 0);
        assert!(matches!(
            lanes.acquire(&route, &metrics).await,
            Err(DispatchError::Timeout)
        ));
        assert_eq!(held.lane.state.lock().unwrap().queued, 0);
        drop(held);
        assert!(lanes.acquire(&route, &metrics).await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn runtime_resize_keeps_one_lane_across_revisions_and_drains_on_shrink() {
        let lanes = DispatchLanes::default();
        let mut route = route();
        let metrics = Metrics::default();
        let first = lanes.acquire(&route, &metrics).await.unwrap();
        let second = lanes.acquire(&route, &metrics).await.unwrap();
        route.transport_revision += 1;
        route.config["transport_policy"]["dispatch_max_in_flight"] = serde_json::json!(1);
        route.config["transport_policy"]["connect_timeout_millis"] = serde_json::json!(1000);
        assert!(matches!(
            lanes.acquire(&route, &metrics).await,
            Err(DispatchError::Timeout)
        ));
        drop(second);
        assert!(matches!(
            lanes.acquire(&route, &metrics).await,
            Err(DispatchError::Timeout)
        ));
        route.transport_revision += 1;
        route.config["transport_policy"]["dispatch_max_in_flight"] = serde_json::json!(2);
        let grown = lanes.acquire(&route, &metrics).await.unwrap();
        assert!(Arc::ptr_eq(&first.lane, &grown.lane));
        assert!(first.matches(&route));
    }

    #[test]
    fn lane_key_ignores_secrets_revision_and_timeouts_but_separates_accounts_endpoints() {
        let mut route = route();
        let proxy = |raw: &str| UpstreamCredential::ProxiedApiKey {
            value: String::new(),
            header: "authorization".into(),
            prefix: String::new(),
            proxy_url: raw.into(),
            proxy_network_scope: crate::network::OutboundScope::Private,
        };
        route.credential = proxy("socks5h://alice:secret@proxy.example:1080");
        let key = LaneKey::new(&route);
        route.credential = proxy("socks5h://bob:rotated@proxy.example:1080");
        route.transport_revision += 1;
        route.config = serde_json::json!({});
        assert!(key == LaneKey::new(&route));
        route.account_id = uuid::Uuid::new_v4();
        assert!(key != LaneKey::new(&route));
        route.account_id = uuid::Uuid::nil();
        route.credential = proxy("socks5h://proxy.example:1081");
        assert!(key != LaneKey::new(&route));
    }

    #[tokio::test]
    async fn fifo_waiters_cannot_be_overtaken_and_active_cancellation_releases_capacity() {
        let lanes = Arc::new(DispatchLanes::default());
        let mut route = route();
        route.config["transport_policy"]["dispatch_max_in_flight"] = serde_json::json!(1);
        route.config["transport_policy"]["dispatch_queue_timeout_millis"] = serde_json::json!(5000);
        let held = lanes.acquire(&route, &Metrics::default()).await.unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut tasks = Vec::new();
        for index in 0..3 {
            let (lanes, route, sender) = (lanes.clone(), route.clone(), sender.clone());
            tasks.push(tokio::spawn(async move {
                let _permit = lanes.acquire(&route, &Metrics::default()).await.unwrap();
                sender.send(index).unwrap();
                if index == 0 {
                    std::future::pending::<()>().await;
                }
            }));
            tokio::task::yield_now().await;
        }
        drop(held);
        assert_eq!(receiver.recv().await, Some(0));
        tasks[0].abort();
        assert_eq!(receiver.recv().await, Some(1));
        assert_eq!(receiver.recv().await, Some(2));
        for task in tasks {
            let _ = task.await;
        }
        assert!(lanes.acquire(&route, &Metrics::default()).await.is_ok());
    }
}
