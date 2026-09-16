use std::{
    collections::BTreeMap,
    fmt::Write,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

mod codex;
pub(crate) mod memory_admission;
pub(crate) mod plugin_execution;

pub(crate) use codex::{CodexBadRequestClassification, CodexBadRequestRetry};

const LATENCY_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0, 600.0,
];

pub const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const BUILD_GIT_SHA: &str = match option_env!("MTC_BUILD_GIT_SHA") {
    Some(value) => value,
    None => "unknown",
};
pub const BUILD_TIMESTAMP: &str = match option_env!("MTC_BUILD_TIMESTAMP") {
    Some(value) => value,
    None => "unknown",
};
pub const BUILD_TARGET: &str = match option_env!("MTC_BUILD_TARGET") {
    Some(value) => value,
    None => "unknown",
};

#[derive(Clone, Default)]
pub struct Metrics {
    inner: Arc<MetricsInner>,
}

struct MetricsInner {
    memory_admission: memory_admission::Counters,
    plugin_execution: plugin_execution::Counters,
    proxy_memory_rejections: [AtomicU64; 6],
    http: Mutex<BTreeMap<HttpLabels, RequestSeries>>,
    upstream: Mutex<BTreeMap<UpstreamLabels, RequestSeries>>,
    upstream_health: Mutex<BTreeMap<UpstreamHealthLabels, u64>>,
    proxy_lifecycle_deadlines: [AtomicU64; ProxyLifecycleDeadlineOutcome::COUNT],
    codex_bad_request_classifications: Mutex<BTreeMap<CodexBadRequestClassification, u64>>,
    codex_bad_request_retries: Mutex<BTreeMap<CodexBadRequestRetry, u64>>,
    active_http_requests: AtomicI64,
    active_streams: [AtomicI64; ActiveStreamKind::COUNT],
    active_upstreams: Mutex<BTreeMap<UpstreamActivityLabels, i64>>,
    component_memory_bytes: [AtomicI64; MemoryComponent::COUNT],
    background_projection_completed: [AtomicU64; BackgroundProjectionKind::COUNT],
    background_projection_failed: [AtomicU64; BackgroundProjectionKind::COUNT],
    profiling: AtomicBool,
    database_ready: AtomicI64,
    archive_ready: AtomicI64,
    readiness: tokio::sync::Mutex<Option<CachedReadiness>>,
    process_started: Instant,
}

impl Default for MetricsInner {
    fn default() -> Self {
        Self {
            memory_admission: memory_admission::Counters::default(),
            plugin_execution: plugin_execution::Counters::default(),
            proxy_memory_rejections: std::array::from_fn(|_| AtomicU64::new(0)),
            http: Mutex::default(),
            upstream: Mutex::default(),
            upstream_health: Mutex::default(),
            proxy_lifecycle_deadlines: std::array::from_fn(|_| AtomicU64::new(0)),
            codex_bad_request_classifications: Mutex::default(),
            codex_bad_request_retries: Mutex::default(),
            active_http_requests: AtomicI64::new(0),
            active_streams: std::array::from_fn(|_| AtomicI64::new(0)),
            active_upstreams: Mutex::default(),
            component_memory_bytes: std::array::from_fn(|_| AtomicI64::new(0)),
            background_projection_completed: std::array::from_fn(|_| AtomicU64::new(0)),
            background_projection_failed: std::array::from_fn(|_| AtomicU64::new(0)),
            profiling: AtomicBool::new(false),
            database_ready: AtomicI64::new(0),
            archive_ready: AtomicI64::new(0),
            readiness: tokio::sync::Mutex::default(),
            process_started: Instant::now(),
        }
    }
}

/// Closed, payload-independent admission stages. Never derive labels from routes.
#[derive(Clone, Copy)]
pub(crate) enum ProxyMemoryRejectionStage {
    Ingress,
    Json,
    Retained,
    Route,
    Plugin,
    Response,
}

impl ProxyMemoryRejectionStage {
    const ALL: [Self; 6] = [
        Self::Ingress,
        Self::Json,
        Self::Retained,
        Self::Route,
        Self::Plugin,
        Self::Response,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Ingress => "ingress",
            Self::Json => "json",
            Self::Retained => "retained",
            Self::Route => "route",
            Self::Plugin => "plugin",
            Self::Response => "response",
        }
    }
}

#[derive(Clone, Copy)]
struct CachedReadiness {
    checked_at: Instant,
    database: bool,
    archive: bool,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct HttpLabels {
    method: &'static str,
    route: String,
    status_class: &'static str,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct UpstreamLabels {
    provider: &'static str,
    operation: &'static str,
    status_class: &'static str,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct UpstreamActivityLabels {
    provider: &'static str,
    operation: &'static str,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct UpstreamHealthLabels {
    event: &'static str,
    reason: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpstreamHealthEvent {
    Failure,
    Skipped,
    Failover,
    Recovered,
}

impl UpstreamHealthEvent {
    const fn label(self) -> &'static str {
        match self {
            Self::Failure => "failure",
            Self::Skipped => "skipped",
            Self::Failover => "failover",
            Self::Recovered => "recovered",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpstreamHealthReason {
    RecoveryWaitCapacity,
    RateLimited,
    Unavailable,
    InvalidResponse,
    Connection,
    Cooldown,
    Success,
}

impl UpstreamHealthReason {
    const fn label(self) -> &'static str {
        match self {
            Self::RecoveryWaitCapacity => "recovery_wait_capacity",
            Self::RateLimited => "rate_limited",
            Self::Unavailable => "unavailable",
            Self::InvalidResponse => "invalid_response",
            Self::Connection => "connection",
            Self::Cooldown => "cooldown",
            Self::Success => "success",
        }
    }
}

/// Fixed outcomes for the absolute request-lifecycle deadline. This avoids
/// labels derived from request, tenant, or upstream identities.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyLifecycleDeadlineOutcome {
    Converged,
    ReconcileFailed,
}

impl ProxyLifecycleDeadlineOutcome {
    const COUNT: usize = 2;

    const fn index(self) -> usize {
        self as usize
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Converged => "converged",
            Self::ReconcileFailed => "reconcile_failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActiveStreamKind {
    ProxyResponse,
    RequestEvents,
}

impl ActiveStreamKind {
    const COUNT: usize = 2;

    const fn index(self) -> usize {
        self as usize
    }

    const fn label(self) -> &'static str {
        match self {
            Self::ProxyResponse => "proxy_response",
            Self::RequestEvents => "request_events",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryComponent {
    RequestBuffer,
    ResponseBuffer,
    StreamCapture,
    ArchiveMultipart,
}

/// Fixed worker projection queues. These labels deliberately exclude tenant,
/// key, reservation, and request identities so retries cannot create metric
/// cardinality proportional to traffic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackgroundProjectionKind {
    MeteredUsage,
    Conversation,
}

impl BackgroundProjectionKind {
    const COUNT: usize = 2;

    const fn index(self) -> usize {
        self as usize
    }

    const fn label(self) -> &'static str {
        match self {
            Self::MeteredUsage => "metered_usage",
            Self::Conversation => "conversation",
        }
    }
}

impl MemoryComponent {
    const COUNT: usize = 4;

    const fn index(self) -> usize {
        self as usize
    }

    const fn label(self) -> &'static str {
        match self {
            Self::RequestBuffer => "request_buffer",
            Self::ResponseBuffer => "response_buffer",
            Self::StreamCapture => "stream_capture",
            Self::ArchiveMultipart => "archive_multipart_reserved",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProfileKind {
    Cpu,
    Heap,
}

enum ActivityTarget {
    HttpRequest,
    Stream(ActiveStreamKind),
    Upstream(UpstreamActivityLabels),
}

#[must_use = "dropping the guard records that the activity ended"]
pub struct ActivityGuard {
    inner: Arc<MetricsInner>,
    target: ActivityTarget,
}

#[must_use = "dropping the guard releases the accounted component memory"]
pub struct MemoryUsageGuard {
    inner: Arc<MetricsInner>,
    component: MemoryComponent,
    bytes: i64,
}

#[must_use = "dropping the guard releases the profiling singleflight"]
pub struct ProfileGuard {
    inner: Arc<MetricsInner>,
}

#[derive(Clone)]
struct RequestSeries {
    requests: u64,
    errors: u64,
    latency_count: u64,
    latency_sum: f64,
    latency_buckets: Vec<u64>,
}

impl Default for RequestSeries {
    fn default() -> Self {
        Self {
            requests: 0,
            errors: 0,
            latency_count: 0,
            latency_sum: 0.0,
            latency_buckets: vec![0; LATENCY_BUCKETS.len()],
        }
    }
}

impl Metrics {
    pub(crate) fn observe_plugin_execution(
        &self,
        phase: plugin_execution::Phase,
        outcome: plugin_execution::Outcome,
    ) {
        self.inner.plugin_execution.observe(phase, outcome);
    }
    pub fn process_runtime_metrics(&self) -> ProcessRuntimeMetrics {
        process_runtime_metrics(self.inner.process_started)
    }

    pub fn active_http_request(&self) -> ActivityGuard {
        increment(&self.inner.active_http_requests, 1);
        ActivityGuard {
            inner: self.inner.clone(),
            target: ActivityTarget::HttpRequest,
        }
    }

    pub fn active_stream(&self, kind: ActiveStreamKind) -> ActivityGuard {
        increment(&self.inner.active_streams[kind.index()], 1);
        ActivityGuard {
            inner: self.inner.clone(),
            target: ActivityTarget::Stream(kind),
        }
    }

    pub fn active_upstream(&self, provider: &str, operation: &'static str) -> ActivityGuard {
        let labels = UpstreamActivityLabels {
            provider: bounded_provider(provider),
            operation: bounded_operation(operation),
        };
        let mut active = self
            .inner
            .active_upstreams
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let value = active.entry(labels.clone()).or_default();
        *value = value.saturating_add(1);
        drop(active);
        ActivityGuard {
            inner: self.inner.clone(),
            target: ActivityTarget::Upstream(labels),
        }
    }

    pub fn memory_usage(&self, component: MemoryComponent, bytes: usize) -> MemoryUsageGuard {
        let bytes = i64::try_from(bytes).unwrap_or(i64::MAX);
        increment(&self.inner.component_memory_bytes[component.index()], bytes);
        MemoryUsageGuard {
            inner: self.inner.clone(),
            component,
            bytes,
        }
    }

    pub(crate) fn record_proxy_memory_rejection(&self, stage: ProxyMemoryRejectionStage) {
        self.inner.proxy_memory_rejections[stage as usize].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn proxy_memory_wait(
        &self,
        stage: memory_admission::Stage,
    ) -> memory_admission::WaitGuard {
        self.inner.memory_admission.wait(stage)
    }

    pub(crate) fn observe_proxy_memory_error(
        &self,
        stage: ProxyMemoryRejectionStage,
        error: crate::error::AppError,
    ) -> crate::error::AppError {
        if matches!(&error, crate::error::AppError::Overloaded) {
            self.record_proxy_memory_rejection(stage);
        }
        error
    }

    /// Records one terminal worker projection attempt with a fixed queue label.
    pub fn observe_background_projection(&self, kind: BackgroundProjectionKind, succeeded: bool) {
        let counter = if succeeded {
            &self.inner.background_projection_completed[kind.index()]
        } else {
            &self.inner.background_projection_failed[kind.index()]
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn try_begin_profile(&self, _kind: ProfileKind) -> Option<ProfileGuard> {
        self.inner
            .profiling
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| ProfileGuard {
                inner: self.inner.clone(),
            })
    }

    pub fn observe_http(
        &self,
        method: &http::Method,
        route: &str,
        status: http::StatusCode,
        elapsed: Duration,
    ) {
        let labels = HttpLabels {
            method: bounded_method(method),
            route: bounded_route(route),
            status_class: status_class(status),
        };
        observe_series(
            &mut self.inner.http.lock().unwrap_or_else(|e| e.into_inner()),
            labels,
            status.is_client_error() || status.is_server_error(),
            elapsed,
        );
    }

    pub fn observe_upstream(
        &self,
        provider: &str,
        operation: &'static str,
        status: Option<http::StatusCode>,
        elapsed: Duration,
    ) {
        let labels = UpstreamLabels {
            provider: bounded_provider(provider),
            operation: bounded_operation(operation),
            status_class: status.map(status_class).unwrap_or("transport_error"),
        };
        let error = status.is_none_or(|value| !value.is_success());
        observe_series(
            &mut self
                .inner
                .upstream
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
            labels,
            error,
            elapsed,
        );
    }

    pub fn observe_upstream_health(
        &self,
        event: UpstreamHealthEvent,
        reason: UpstreamHealthReason,
    ) {
        let mut values = self
            .inner
            .upstream_health
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let value = values
            .entry(UpstreamHealthLabels {
                event: event.label(),
                reason: reason.label(),
            })
            .or_default();
        *value = value.saturating_add(1);
    }

    pub fn observe_proxy_lifecycle_deadline(&self, outcome: ProxyLifecycleDeadlineOutcome) {
        self.inner.proxy_lifecycle_deadlines[outcome.index()].fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_dependency_ready(&self, dependency: &'static str, ready: bool) {
        let value = i64::from(ready);
        match dependency {
            "database" => self.inner.database_ready.store(value, Ordering::Relaxed),
            "archive" => self.inner.archive_ready.store(value, Ordering::Relaxed),
            _ => {}
        }
    }

    /// Coalesces concurrent readiness probes and caches their result briefly.
    /// This keeps anonymous Kubernetes probes from amplifying into an S3 list
    /// and SQL query on every inbound request while still detecting dependency
    /// failures quickly enough for endpoint removal.
    pub async fn readiness<F, Fut>(&self, check: F) -> (bool, bool)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = (bool, bool)>,
    {
        const TTL: Duration = Duration::from_secs(5);
        let Ok(mut cached) = self.inner.readiness.try_lock() else {
            // Never let a probe burst build a waiter queue while one dependency
            // check is already running. Until the first check completes this is
            // conservatively not-ready; afterwards it is the most recent result.
            return (
                self.inner.database_ready.load(Ordering::Relaxed) == 1,
                self.inner.archive_ready.load(Ordering::Relaxed) == 1,
            );
        };
        if let Some(value) = *cached
            && value.checked_at.elapsed() < TTL
        {
            return (value.database, value.archive);
        }
        // Deliberately hold this mutex across the checks: it is a singleflight
        // lock used only by /readyz, never by request processing or /metrics.
        let (database, archive) = check().await;
        *cached = Some(CachedReadiness {
            checked_at: Instant::now(),
            database,
            archive,
        });
        drop(cached);
        self.set_dependency_ready("database", database);
        self.set_dependency_ready("archive", archive);
        (database, archive)
    }

    pub fn render(&self, runtime: &RuntimeMetrics) -> String {
        let http = self
            .inner
            .http
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let upstream = self
            .inner
            .upstream
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let upstream_health = self
            .inner
            .upstream_health
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let codex_bad_request_classifications = self
            .inner
            .codex_bad_request_classifications
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let codex_bad_request_retries = self
            .inner
            .codex_bad_request_retries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut output = String::with_capacity(16 * 1024);
        self.inner.memory_admission.render(&mut output);
        self.inner.plugin_execution.render(&mut output);
        output.push_str("# HELP memeloop_token_center_proxy_memory_rejections_total Capacity rejections by fixed admission stage.\n");
        output.push_str("# TYPE memeloop_token_center_proxy_memory_rejections_total counter\n");
        for stage in ProxyMemoryRejectionStage::ALL {
            let _ = writeln!(
                output,
                "memeloop_token_center_proxy_memory_rejections_total{{stage=\"{}\"}} {}",
                stage.label(),
                self.inner.proxy_memory_rejections[stage as usize].load(Ordering::Relaxed)
            );
        }

        output
            .push_str("# HELP memeloop_token_center_build_info Build metadata for this binary.\n");
        output.push_str("# TYPE memeloop_token_center_build_info gauge\n");
        let _ = writeln!(
            output,
            "memeloop_token_center_build_info{{version=\"{}\",revision=\"{}\",target=\"{}\"}} 1",
            prometheus_escape(BUILD_VERSION),
            prometheus_escape(BUILD_GIT_SHA),
            prometheus_escape(BUILD_TARGET),
        );

        output.push_str("# HELP memeloop_token_center_dependency_ready Whether a required dependency most recently passed a health check.\n");
        output.push_str("# TYPE memeloop_token_center_dependency_ready gauge\n");
        let _ = writeln!(
            output,
            "memeloop_token_center_dependency_ready{{dependency=\"database\"}} {}",
            self.inner.database_ready.load(Ordering::Relaxed)
        );
        let _ = writeln!(
            output,
            "memeloop_token_center_dependency_ready{{dependency=\"archive\"}} {}",
            self.inner.archive_ready.load(Ordering::Relaxed)
        );

        render_http(&mut output, &http);
        render_upstream(&mut output, &upstream);
        render_upstream_health(&mut output, &upstream_health);
        render_proxy_lifecycle_deadlines(&mut output, &self.inner);
        codex::render_bad_requests(
            &mut output,
            &codex_bad_request_classifications,
            &codex_bad_request_retries,
        );
        render_active(&mut output, &self.inner);
        render_background_projections(&mut output, &self.inner);
        render_runtime(&mut output, runtime);
        render_process(&mut output, &self.inner);
        render_allocator(&mut output);
        output
    }
}

fn render_upstream_health(output: &mut String, values: &BTreeMap<UpstreamHealthLabels, u64>) {
    output.push_str("# HELP memeloop_token_center_upstream_candidate_health_events_total Account candidate circuit-breaker events with fixed low-cardinality labels.\n");
    output
        .push_str("# TYPE memeloop_token_center_upstream_candidate_health_events_total counter\n");
    for (labels, value) in values {
        let _ = writeln!(
            output,
            "memeloop_token_center_upstream_candidate_health_events_total{{event=\"{}\",reason=\"{}\"}} {value}",
            labels.event, labels.reason
        );
    }
}

fn render_proxy_lifecycle_deadlines(output: &mut String, inner: &MetricsInner) {
    output.push_str("# HELP memeloop_token_center_proxy_lifecycle_deadline_events_total Absolute proxy lifecycle deadline outcomes with fixed low-cardinality labels.\n");
    output.push_str("# TYPE memeloop_token_center_proxy_lifecycle_deadline_events_total counter\n");
    for outcome in [
        ProxyLifecycleDeadlineOutcome::Converged,
        ProxyLifecycleDeadlineOutcome::ReconcileFailed,
    ] {
        let value = inner.proxy_lifecycle_deadlines[outcome.index()].load(Ordering::Relaxed);
        let _ = writeln!(
            output,
            "memeloop_token_center_proxy_lifecycle_deadline_events_total{{outcome=\"{}\"}} {value}",
            outcome.label(),
        );
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        match &self.target {
            ActivityTarget::HttpRequest => increment(&self.inner.active_http_requests, -1),
            ActivityTarget::Stream(kind) => increment(&self.inner.active_streams[kind.index()], -1),
            ActivityTarget::Upstream(labels) => {
                let mut active = self
                    .inner
                    .active_upstreams
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if let Some(value) = active.get_mut(labels) {
                    *value = value.saturating_sub(1);
                }
            }
        }
    }
}

impl MemoryUsageGuard {
    pub fn set_bytes(&mut self, bytes: usize) {
        let bytes = i64::try_from(bytes).unwrap_or(i64::MAX);
        let difference = bytes.saturating_sub(self.bytes);
        increment(
            &self.inner.component_memory_bytes[self.component.index()],
            difference,
        );
        self.bytes = bytes;
    }
}

impl Drop for MemoryUsageGuard {
    fn drop(&mut self) {
        increment(
            &self.inner.component_memory_bytes[self.component.index()],
            self.bytes.saturating_neg(),
        );
    }
}

impl Drop for ProfileGuard {
    fn drop(&mut self) {
        self.inner.profiling.store(false, Ordering::Release);
    }
}

fn increment(value: &AtomicI64, difference: i64) {
    let _ = value.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(difference).max(0))
    });
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DatabaseRuntimeMetrics {
    pub pool_size: u32,
    pub pool_idle: usize,
    pub queued_jobs: i64,
    pub running_jobs: i64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimeMetrics {
    pub database: Option<DatabaseRuntimeMetrics>,
    pub request_event_streams: usize,
    pub gateway_body_rejections: [[u64; 3]; 4],
    pub proxy_memory_used_bytes: usize,
    pub proxy_memory_limit_bytes: usize,
    pub retained_request_memory_used_bytes: usize,
    pub retained_request_memory_limit_bytes: usize,
    pub gateway_body_reads: usize,
    pub responses_request_spools: usize,
    pub responses_request_spool_used_bytes: usize,
    pub responses_request_spool_limit_bytes: usize,
    pub responses_request_spool_capacity_rejections: u64,
    pub proxy_lifecycles: usize,
    pub proxy_archive_streams: usize,
    pub plugin_cache_entries: usize,
    pub plugin_cache_bytes: usize,
    pub loaded_plugins: usize,
}

fn observe_series<K: Ord>(
    series: &mut BTreeMap<K, RequestSeries>,
    labels: K,
    error: bool,
    elapsed: Duration,
) {
    let seconds = elapsed.as_secs_f64();
    let value = series.entry(labels).or_default();
    value.requests = value.requests.saturating_add(1);
    value.errors = value.errors.saturating_add(u64::from(error));
    value.latency_count = value.latency_count.saturating_add(1);
    value.latency_sum += seconds;
    for (index, upper_bound) in LATENCY_BUCKETS.iter().enumerate() {
        if seconds <= *upper_bound {
            value.latency_buckets[index] = value.latency_buckets[index].saturating_add(1);
        }
    }
}

fn render_http(output: &mut String, values: &BTreeMap<HttpLabels, RequestSeries>) {
    output.push_str("# HELP memeloop_token_center_http_requests_total HTTP requests completed by method, route template and status class.\n");
    output.push_str("# TYPE memeloop_token_center_http_requests_total counter\n");
    output.push_str(
        "# HELP memeloop_token_center_http_request_errors_total HTTP 4xx and 5xx responses.\n",
    );
    output.push_str("# TYPE memeloop_token_center_http_request_errors_total counter\n");
    output.push_str("# HELP memeloop_token_center_http_request_duration_seconds Time until response headers are produced.\n");
    output.push_str("# TYPE memeloop_token_center_http_request_duration_seconds histogram\n");
    for (labels, value) in values {
        let base = format!(
            "method=\"{}\",route=\"{}\",status_class=\"{}\"",
            labels.method,
            prometheus_escape(&labels.route),
            labels.status_class
        );
        let _ = writeln!(
            output,
            "memeloop_token_center_http_requests_total{{{base}}} {}",
            value.requests
        );
        let _ = writeln!(
            output,
            "memeloop_token_center_http_request_errors_total{{{base}}} {}",
            value.errors
        );
        render_histogram(
            output,
            "memeloop_token_center_http_request_duration_seconds",
            &base,
            value,
        );
    }
}

fn render_upstream(output: &mut String, values: &BTreeMap<UpstreamLabels, RequestSeries>) {
    output.push_str("# HELP memeloop_token_center_upstream_requests_total Upstream HTTP attempts by bounded provider class and operation.\n");
    output.push_str("# TYPE memeloop_token_center_upstream_requests_total counter\n");
    output.push_str("# HELP memeloop_token_center_upstream_errors_total Non-success upstream responses and transport errors.\n");
    output.push_str("# TYPE memeloop_token_center_upstream_errors_total counter\n");
    output.push_str("# HELP memeloop_token_center_upstream_request_duration_seconds Upstream request latency until response headers.\n");
    output.push_str("# TYPE memeloop_token_center_upstream_request_duration_seconds histogram\n");
    for (labels, value) in values {
        let base = format!(
            "provider=\"{}\",operation=\"{}\",status_class=\"{}\"",
            labels.provider, labels.operation, labels.status_class
        );
        let _ = writeln!(
            output,
            "memeloop_token_center_upstream_requests_total{{{base}}} {}",
            value.requests
        );
        let _ = writeln!(
            output,
            "memeloop_token_center_upstream_errors_total{{{base}}} {}",
            value.errors
        );
        render_histogram(
            output,
            "memeloop_token_center_upstream_request_duration_seconds",
            &base,
            value,
        );
    }
}

fn render_histogram(output: &mut String, name: &str, base: &str, value: &RequestSeries) {
    for (index, upper_bound) in LATENCY_BUCKETS.iter().enumerate() {
        let _ = writeln!(
            output,
            "{name}_bucket{{{base},le=\"{upper_bound}\"}} {}",
            value.latency_buckets[index]
        );
    }
    let _ = writeln!(
        output,
        "{name}_bucket{{{base},le=\"+Inf\"}} {}",
        value.latency_count
    );
    let _ = writeln!(output, "{name}_sum{{{base}}} {}", value.latency_sum);
    let _ = writeln!(output, "{name}_count{{{base}}} {}", value.latency_count);
}

fn render_active(output: &mut String, inner: &MetricsInner) {
    output.push_str(
        "# HELP memeloop_token_center_http_active_requests HTTP handlers currently executing.\n",
    );
    output.push_str("# TYPE memeloop_token_center_http_active_requests gauge\n");
    let _ = writeln!(
        output,
        "memeloop_token_center_http_active_requests {}",
        inner.active_http_requests.load(Ordering::Relaxed)
    );

    output.push_str(
        "# HELP memeloop_token_center_active_streams Long-lived application streams currently open.\n",
    );
    output.push_str("# TYPE memeloop_token_center_active_streams gauge\n");
    for kind in [
        ActiveStreamKind::ProxyResponse,
        ActiveStreamKind::RequestEvents,
    ] {
        let _ = writeln!(
            output,
            "memeloop_token_center_active_streams{{kind=\"{}\"}} {}",
            kind.label(),
            inner.active_streams[kind.index()].load(Ordering::Relaxed)
        );
    }

    output.push_str(
        "# HELP memeloop_token_center_upstream_active_requests Upstream HTTP exchanges that have not released their response body.\n",
    );
    output.push_str("# TYPE memeloop_token_center_upstream_active_requests gauge\n");
    let active_upstreams = inner
        .active_upstreams
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for (labels, value) in active_upstreams.iter() {
        let _ = writeln!(
            output,
            "memeloop_token_center_upstream_active_requests{{provider=\"{}\",operation=\"{}\"}} {value}",
            labels.provider, labels.operation
        );
    }

    output.push_str(
        "# HELP memeloop_token_center_component_memory_bytes Application-owned or reserved bytes by fixed component.\n",
    );
    output.push_str("# TYPE memeloop_token_center_component_memory_bytes gauge\n");
    for component in [
        MemoryComponent::RequestBuffer,
        MemoryComponent::ResponseBuffer,
        MemoryComponent::StreamCapture,
        MemoryComponent::ArchiveMultipart,
    ] {
        let _ = writeln!(
            output,
            "memeloop_token_center_component_memory_bytes{{component=\"{}\"}} {}",
            component.label(),
            inner.component_memory_bytes[component.index()].load(Ordering::Relaxed)
        );
    }
}

fn render_background_projections(output: &mut String, inner: &MetricsInner) {
    output.push_str(
        "# HELP memeloop_token_center_background_projections_total Worker projection attempts by fixed queue and outcome.\n",
    );
    output.push_str("# TYPE memeloop_token_center_background_projections_total counter\n");
    for kind in [
        BackgroundProjectionKind::MeteredUsage,
        BackgroundProjectionKind::Conversation,
    ] {
        let _ = writeln!(
            output,
            "memeloop_token_center_background_projections_total{{queue=\"{}\",outcome=\"completed\"}} {}",
            kind.label(),
            inner.background_projection_completed[kind.index()].load(Ordering::Relaxed),
        );
        let _ = writeln!(
            output,
            "memeloop_token_center_background_projections_total{{queue=\"{}\",outcome=\"failed\"}} {}",
            kind.label(),
            inner.background_projection_failed[kind.index()].load(Ordering::Relaxed),
        );
    }
}

fn render_runtime(output: &mut String, runtime: &RuntimeMetrics) {
    output.push_str(
        "# HELP memeloop_token_center_db_pool_connections Current SQL pool connections.\n",
    );
    output.push_str("# TYPE memeloop_token_center_db_pool_connections gauge\n");
    output.push_str("# HELP memeloop_token_center_generation_jobs Current asynchronous generation jobs by active status.\n");
    output.push_str("# TYPE memeloop_token_center_generation_jobs gauge\n");
    if let Some(database) = runtime.database {
        let in_use = usize::try_from(database.pool_size)
            .unwrap_or(usize::MAX)
            .saturating_sub(database.pool_idle);
        let _ = writeln!(
            output,
            "memeloop_token_center_db_pool_connections{{state=\"idle\"}} {}",
            database.pool_idle
        );
        let _ = writeln!(
            output,
            "memeloop_token_center_db_pool_connections{{state=\"in_use\"}} {in_use}"
        );
        let _ = writeln!(
            output,
            "memeloop_token_center_generation_jobs{{status=\"queued\"}} {}",
            database.queued_jobs
        );
        let _ = writeln!(
            output,
            "memeloop_token_center_generation_jobs{{status=\"running\"}} {}",
            database.running_jobs
        );
    }

    output.push_str("# HELP memeloop_token_center_background_work_items In-process active work by fixed queue or capacity class.\n");
    output.push_str("# TYPE memeloop_token_center_background_work_items gauge\n");
    for (queue, value) in [
        ("request_event_streams", runtime.request_event_streams),
        ("gateway_body_reads", runtime.gateway_body_reads),
        ("responses_request_spools", runtime.responses_request_spools),
        ("proxy_lifecycles", runtime.proxy_lifecycles),
        ("proxy_archive_streams", runtime.proxy_archive_streams),
    ] {
        let _ = writeln!(
            output,
            "memeloop_token_center_background_work_items{{queue=\"{queue}\",state=\"active\"}} {value}"
        );
    }
    output.push_str(
        "# HELP memeloop_token_center_gateway_body_rejections_total Rejected gateway request bodies by fixed route class and reason.\n",
    );
    output.push_str("# TYPE memeloop_token_center_gateway_body_rejections_total counter\n");
    for route_class in crate::gateway_body::GatewayBodyRouteClass::ALL {
        for reason in crate::gateway_body::GatewayBodyRejectionReason::ALL {
            let value = runtime.gateway_body_rejections[route_class.index()][reason.index()];
            let _ = writeln!(
                output,
                "memeloop_token_center_gateway_body_rejections_total{{route_class=\"{}\",reason=\"{}\"}} {value}",
                route_class.label(),
                reason.label(),
            );
        }
    }
    output.push_str(
        "# HELP memeloop_token_center_request_spool_bytes Process-local node-disk request spool bytes.\n",
    );
    output.push_str("# TYPE memeloop_token_center_request_spool_bytes gauge\n");
    let _ = writeln!(
        output,
        "memeloop_token_center_request_spool_bytes{{measure=\"used\"}} {}",
        runtime.responses_request_spool_used_bytes
    );
    let _ = writeln!(
        output,
        "memeloop_token_center_request_spool_bytes{{measure=\"limit\"}} {}",
        runtime.responses_request_spool_limit_bytes
    );
    output.push_str(
        "# HELP memeloop_token_center_request_spool_capacity_rejections_total Responses request spools rejected by the fixed process-local disk budget.\n",
    );
    output
        .push_str("# TYPE memeloop_token_center_request_spool_capacity_rejections_total counter\n");
    let _ = writeln!(
        output,
        "memeloop_token_center_request_spool_capacity_rejections_total {}",
        runtime.responses_request_spool_capacity_rejections
    );
    output.push_str("# HELP memeloop_token_center_proxy_memory_bytes Actual weighted memory admission permits by fixed pool and measure.\n");
    output.push_str("# TYPE memeloop_token_center_proxy_memory_bytes gauge\n");
    for (pool, used, limit) in [
        (
            "lifecycle",
            runtime.proxy_memory_used_bytes,
            runtime.proxy_memory_limit_bytes,
        ),
        (
            "retained_request",
            runtime.retained_request_memory_used_bytes,
            runtime.retained_request_memory_limit_bytes,
        ),
    ] {
        let _ = writeln!(
            output,
            "memeloop_token_center_proxy_memory_bytes{{pool=\"{pool}\",measure=\"used\"}} {used}"
        );
        let _ = writeln!(
            output,
            "memeloop_token_center_proxy_memory_bytes{{pool=\"{pool}\",measure=\"limit\"}} {limit}"
        );
    }
    output.push_str(
        "# HELP memeloop_token_center_plugin_cache_entries Resolved plugin configuration and service-data cache entries.\n",
    );
    output.push_str("# TYPE memeloop_token_center_plugin_cache_entries gauge\n");
    let _ = writeln!(
        output,
        "memeloop_token_center_plugin_cache_entries {}",
        runtime.plugin_cache_entries
    );
    output.push_str(
        "# HELP memeloop_token_center_plugin_cache_bytes Estimated bytes retained by plugin configuration and service-data caches.\n",
    );
    output.push_str("# TYPE memeloop_token_center_plugin_cache_bytes gauge\n");
    let _ = writeln!(
        output,
        "memeloop_token_center_plugin_cache_bytes {}",
        runtime.plugin_cache_bytes
    );
    output.push_str("# HELP memeloop_token_center_plugins_loaded Loaded WebAssembly plugins.\n");
    output.push_str("# TYPE memeloop_token_center_plugins_loaded gauge\n");
    let _ = writeln!(
        output,
        "memeloop_token_center_plugins_loaded {}",
        runtime.loaded_plugins
    );
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct ProcessRuntimeMetrics {
    pub resident_memory_bytes: Option<u64>,
    pub cpu_seconds: Option<f64>,
    pub uptime_seconds: f64,
}

pub fn process_runtime_metrics(process_started: Instant) -> ProcessRuntimeMetrics {
    ProcessRuntimeMetrics {
        resident_memory_bytes: linux_resident_memory_bytes(),
        cpu_seconds: linux_cpu_seconds(),
        uptime_seconds: process_started.elapsed().as_secs_f64(),
    }
}

fn render_process(output: &mut String, inner: &MetricsInner) {
    let process = process_runtime_metrics(inner.process_started);
    output.push_str(
        "# HELP process_resident_memory_bytes Resident set size read from the current process.\n",
    );
    output.push_str("# TYPE process_resident_memory_bytes gauge\n");
    if let Some(bytes) = process.resident_memory_bytes {
        let _ = writeln!(output, "process_resident_memory_bytes {bytes}");
    }
    output.push_str(
        "# HELP process_cpu_seconds_total Total CPU time consumed by the current process.\n",
    );
    output.push_str("# TYPE process_cpu_seconds_total counter\n");
    if let Some(seconds) = process.cpu_seconds {
        let _ = writeln!(output, "process_cpu_seconds_total {seconds}");
    }
    output.push_str(
        "# HELP process_start_time_seconds Approximate Unix start time of the current process.\n",
    );
    output.push_str("# TYPE process_start_time_seconds gauge\n");
    let start_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        - process.uptime_seconds;
    let _ = writeln!(output, "process_start_time_seconds {start_time}");
}

#[cfg(target_os = "linux")]
fn linux_resident_memory_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kibibytes = status.lines().find_map(|line| {
        line.strip_prefix("VmRSS:")?
            .split_ascii_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
    })?;
    kibibytes.checked_mul(1024)
}

#[cfg(not(target_os = "linux"))]
fn linux_resident_memory_bytes() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn linux_cpu_seconds() -> Option<f64> {
    let value = rustix::time::clock_gettime(rustix::time::ClockId::ProcessCPUTime);
    let seconds = value.tv_sec as f64 + value.tv_nsec as f64 / 1_000_000_000.0;
    seconds.is_finite().then_some(seconds)
}

#[cfg(not(target_os = "linux"))]
fn linux_cpu_seconds() -> Option<f64> {
    None
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct AllocatorRuntimeMetrics {
    pub allocated_bytes: Option<usize>,
    pub active_bytes: Option<usize>,
    pub resident_bytes: Option<usize>,
    pub mapped_bytes: Option<usize>,
    pub retained_bytes: Option<usize>,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct NativeAllocatorRuntimeMetrics {
    pub arena_bytes: Option<usize>,
    pub allocated_bytes: Option<usize>,
    pub free_bytes: Option<usize>,
    pub mmap_bytes: Option<usize>,
    pub releasable_bytes: Option<usize>,
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[repr(C)]
struct MallInfo2 {
    arena: usize,
    _ordinary_free_blocks: usize,
    _small_free_blocks: usize,
    _mmap_regions: usize,
    mmap_bytes: usize,
    _maximum_allocated: usize,
    _small_free_bytes: usize,
    allocated_bytes: usize,
    free_bytes: usize,
    releasable_bytes: usize,
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
unsafe extern "C" {
    fn mallinfo2() -> MallInfo2;
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn native_allocator_runtime_metrics() -> NativeAllocatorRuntimeMetrics {
    // Rust allocations use the configured prefixed jemalloc; mallinfo2 gives
    // an independent main-arena signal for glibc-backed native dependencies.
    let native = unsafe { mallinfo2() };
    NativeAllocatorRuntimeMetrics {
        arena_bytes: Some(native.arena),
        allocated_bytes: Some(native.allocated_bytes),
        free_bytes: Some(native.free_bytes),
        mmap_bytes: Some(native.mmap_bytes),
        releasable_bytes: Some(native.releasable_bytes),
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn native_allocator_runtime_metrics() -> NativeAllocatorRuntimeMetrics {
    NativeAllocatorRuntimeMetrics::default()
}

#[cfg(not(target_env = "msvc"))]
pub fn allocator_runtime_metrics() -> AllocatorRuntimeMetrics {
    if crate::jemalloc_control::advance_epoch().is_err() {
        return AllocatorRuntimeMetrics::default();
    }
    AllocatorRuntimeMetrics {
        allocated_bytes: crate::jemalloc_control::read_usize(b"stats.allocated\0"),
        active_bytes: crate::jemalloc_control::read_usize(b"stats.active\0"),
        resident_bytes: crate::jemalloc_control::read_usize(b"stats.resident\0"),
        mapped_bytes: crate::jemalloc_control::read_usize(b"stats.mapped\0"),
        retained_bytes: crate::jemalloc_control::read_usize(b"stats.retained\0"),
    }
}

#[cfg(target_env = "msvc")]
pub fn allocator_runtime_metrics() -> AllocatorRuntimeMetrics {
    AllocatorRuntimeMetrics::default()
}

fn render_allocator(output: &mut String) {
    let allocator = allocator_runtime_metrics();
    output.push_str(
        "# HELP memeloop_token_center_allocator_bytes jemalloc memory by fixed allocator state.\n",
    );
    output.push_str("# TYPE memeloop_token_center_allocator_bytes gauge\n");
    for (state, value) in [
        ("allocated", allocator.allocated_bytes),
        ("active", allocator.active_bytes),
        ("resident", allocator.resident_bytes),
        ("mapped", allocator.mapped_bytes),
        ("retained", allocator.retained_bytes),
    ] {
        if let Some(value) = value {
            let _ = writeln!(
                output,
                "memeloop_token_center_allocator_bytes{{state=\"{state}\"}} {value}"
            );
        }
    }
    let native = native_allocator_runtime_metrics();
    output.push_str(
        "# HELP memeloop_token_center_native_allocator_bytes glibc main-arena allocator accounting for native dependencies.\n",
    );
    output.push_str("# TYPE memeloop_token_center_native_allocator_bytes gauge\n");
    for (state, value) in [
        ("arena", native.arena_bytes),
        ("allocated", native.allocated_bytes),
        ("free", native.free_bytes),
        ("mmap", native.mmap_bytes),
        ("releasable", native.releasable_bytes),
    ] {
        if let Some(value) = value {
            let _ = writeln!(
                output,
                "memeloop_token_center_native_allocator_bytes{{state=\"{state}\"}} {value}"
            );
        }
    }
}

fn bounded_method(method: &http::Method) -> &'static str {
    match method.as_str() {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "PATCH" => "PATCH",
        "DELETE" => "DELETE",
        "HEAD" => "HEAD",
        "OPTIONS" => "OPTIONS",
        _ => "OTHER",
    }
}

fn bounded_route(route: &str) -> String {
    if route.len() <= 160 && route.starts_with('/') {
        route.to_owned()
    } else {
        "unmatched".to_owned()
    }
}

fn bounded_provider(provider: &str) -> &'static str {
    match provider {
        "http-json" => "http-json",
        "cbcnx" => "cbcnx",
        "comfyui" => "comfyui",
        "volcengine-seedance" => "volcengine-seedance",
        "legacy" => "legacy",
        value if value.starts_with("plugin:") => "plugin",
        _ => "other",
    }
}

fn bounded_operation(operation: &str) -> &'static str {
    match operation {
        "proxy" => "proxy",
        "component_provider" => "component_provider",
        "image" => "image",
        "generation_submit" => "generation_submit",
        "generation_poll" => "generation_poll",
        "generation_cancel" => "generation_cancel",
        "generation_asset" => "generation_asset",
        _ => "other",
    }
}

fn status_class(status: http::StatusCode) -> &'static str {
    match status.as_u16() / 100 {
        1 => "1xx",
        2 => "2xx",
        3 => "3xx",
        4 => "4xx",
        5 => "5xx",
        _ => "other",
    }
}

fn prometheus_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn route_labels_never_include_unmatched_user_paths() {
        let metrics = Metrics::default();
        metrics.observe_http(
            &http::Method::GET,
            "not-a-template/user-secret",
            http::StatusCode::NOT_FOUND,
            Duration::from_millis(8),
        );
        let rendered = metrics.render(&RuntimeMetrics::default());
        assert!(rendered.contains("route=\"unmatched\""));
        assert!(!rendered.contains("user-secret"));
    }

    #[test]
    fn unknown_provider_values_are_bounded() {
        let metrics = Metrics::default();
        metrics.observe_upstream(
            "tenant-controlled-provider-name",
            "proxy",
            None,
            Duration::from_millis(12),
        );
        let rendered = metrics.render(&RuntimeMetrics::default());
        assert!(rendered.contains("provider=\"other\""));
        assert!(!rendered.contains("tenant-controlled-provider-name"));
        assert!(rendered.contains("status_class=\"transport_error\""));
    }

    #[test]
    fn background_projection_labels_are_fixed() {
        let metrics = Metrics::default();
        metrics.observe_background_projection(BackgroundProjectionKind::MeteredUsage, true);
        metrics.observe_background_projection(BackgroundProjectionKind::Conversation, false);
        let rendered = metrics.render(&RuntimeMetrics::default());
        assert!(rendered.contains(
            "memeloop_token_center_background_projections_total{queue=\"metered_usage\",outcome=\"completed\"} 1"
        ));
        assert!(rendered.contains(
            "memeloop_token_center_background_projections_total{queue=\"conversation\",outcome=\"failed\"} 1"
        ));
    }

    #[test]
    fn upstream_health_labels_are_fixed() {
        let metrics = Metrics::default();
        metrics.observe_upstream_health(
            UpstreamHealthEvent::Failure,
            UpstreamHealthReason::InvalidResponse,
        );
        metrics.observe_upstream_health(
            UpstreamHealthEvent::Failover,
            UpstreamHealthReason::InvalidResponse,
        );
        let rendered = metrics.render(&RuntimeMetrics::default());
        assert!(rendered.contains(
            "memeloop_token_center_upstream_candidate_health_events_total{event=\"failure\",reason=\"invalid_response\"} 1"
        ));
        assert!(rendered.contains(
            "memeloop_token_center_upstream_candidate_health_events_total{event=\"failover\",reason=\"invalid_response\"} 1"
        ));
    }

    #[test]
    fn proxy_lifecycle_deadline_labels_are_fixed() {
        let metrics = Metrics::default();
        metrics.observe_proxy_lifecycle_deadline(ProxyLifecycleDeadlineOutcome::Converged);
        let rendered = metrics.render(&RuntimeMetrics::default());
        assert!(rendered.contains(
            "memeloop_token_center_proxy_lifecycle_deadline_events_total{outcome=\"converged\"} 1"
        ));
        assert!(rendered.contains(
            "memeloop_token_center_proxy_lifecycle_deadline_events_total{outcome=\"reconcile_failed\"} 0"
        ));
    }

    #[test]
    fn codex_bad_request_labels_are_fixed_and_body_free() {
        let metrics = Metrics::default();
        metrics.observe_codex_bad_request_classification(
            CodexBadRequestClassification::DefiniteTransient,
        );
        metrics.observe_codex_bad_request_classification(
            CodexBadRequestClassification::UnclassifiableInvalidJson,
        );
        metrics.observe_codex_bad_request_retry(CodexBadRequestRetry::Started);
        metrics.observe_codex_bad_request_retry(CodexBadRequestRetry::Succeeded);
        let rendered = metrics.render(&RuntimeMetrics::default());
        assert!(rendered.contains(
            "memeloop_token_center_codex_bad_request_classifications_total{classification=\"retryable\"} 1"
        ));
        assert!(rendered.contains(
            "memeloop_token_center_codex_bad_request_classifications_total{classification=\"invalid_json\"} 1"
        ));
        assert!(rendered.contains(
            "memeloop_token_center_codex_bad_request_retries_total{outcome=\"started\"} 1"
        ));
        assert!(rendered.contains(
            "memeloop_token_center_codex_bad_request_retries_total{outcome=\"succeeded\"} 1"
        ));
    }

    #[tokio::test]
    async fn readiness_checks_are_cached() {
        let metrics = Metrics::default();
        let calls = AtomicUsize::new(0);
        let first = metrics
            .readiness(|| async {
                calls.fetch_add(1, Ordering::Relaxed);
                (true, true)
            })
            .await;
        let second = metrics
            .readiness(|| async {
                calls.fetch_add(1, Ordering::Relaxed);
                (false, false)
            })
            .await;
        assert_eq!(first, (true, true));
        assert_eq!(second, (true, true));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn activity_and_memory_guards_release_exactly_once() {
        let metrics = Metrics::default();
        let request = metrics.active_http_request();
        let stream = metrics.active_stream(ActiveStreamKind::ProxyResponse);
        let upstream = metrics.active_upstream("http-json", "proxy");
        let mut memory = metrics.memory_usage(MemoryComponent::StreamCapture, 8);
        memory.set_bytes(13);
        let rendered = metrics.render(&RuntimeMetrics::default());
        assert!(rendered.contains("memeloop_token_center_http_active_requests 1"));
        assert!(
            rendered.contains("memeloop_token_center_active_streams{kind=\"proxy_response\"} 1")
        );
        assert!(rendered.contains(
            "memeloop_token_center_upstream_active_requests{provider=\"http-json\",operation=\"proxy\"} 1"
        ));
        assert!(rendered.contains(
            "memeloop_token_center_component_memory_bytes{component=\"stream_capture\"} 13"
        ));
        drop((request, stream, upstream, memory));
        let rendered = metrics.render(&RuntimeMetrics::default());
        assert!(rendered.contains("memeloop_token_center_http_active_requests 0"));
        assert!(
            rendered.contains("memeloop_token_center_active_streams{kind=\"proxy_response\"} 0")
        );
        assert!(rendered.contains(
            "memeloop_token_center_upstream_active_requests{provider=\"http-json\",operation=\"proxy\"} 0"
        ));
        assert!(rendered.contains(
            "memeloop_token_center_component_memory_bytes{component=\"stream_capture\"} 0"
        ));
    }

    #[test]
    fn profiling_is_a_process_wide_singleflight() {
        let metrics = Metrics::default();
        let first = metrics.try_begin_profile(ProfileKind::Cpu).unwrap();
        assert!(metrics.try_begin_profile(ProfileKind::Cpu).is_none());
        assert!(metrics.try_begin_profile(ProfileKind::Heap).is_none());
        drop(first);
        assert!(metrics.try_begin_profile(ProfileKind::Heap).is_some());
    }

    #[test]
    fn memory_admission_gauges_report_fixed_pools_without_dynamic_labels() {
        let rendered = Metrics::default().render(&RuntimeMetrics {
            proxy_memory_used_bytes: 65536,
            proxy_memory_limit_bytes: 268435456,
            retained_request_memory_used_bytes: 65536,
            retained_request_memory_limit_bytes: 67108864,
            ..RuntimeMetrics::default()
        });
        let gauges: Vec<_> = rendered
            .lines()
            .filter(|line| line.starts_with("memeloop_token_center_proxy_memory_bytes{"))
            .collect();
        assert_eq!(
            gauges,
            [
                "memeloop_token_center_proxy_memory_bytes{pool=\"lifecycle\",measure=\"used\"} 65536",
                "memeloop_token_center_proxy_memory_bytes{pool=\"lifecycle\",measure=\"limit\"} 268435456",
                "memeloop_token_center_proxy_memory_bytes{pool=\"retained_request\",measure=\"used\"} 65536",
                "memeloop_token_center_proxy_memory_bytes{pool=\"retained_request\",measure=\"limit\"} 67108864",
            ]
        );
    }

    #[test]
    fn request_spool_metrics_report_fixed_process_local_series() {
        let rendered = Metrics::default().render(&RuntimeMetrics {
            responses_request_spools: 2,
            responses_request_spool_used_bytes: 25_788_305,
            responses_request_spool_limit_bytes: 134_217_728,
            responses_request_spool_capacity_rejections: 3,
            ..RuntimeMetrics::default()
        });
        assert!(rendered.contains(
            "memeloop_token_center_background_work_items{queue=\"responses_request_spools\",state=\"active\"} 2"
        ));
        assert!(
            rendered
                .contains("memeloop_token_center_request_spool_bytes{measure=\"used\"} 25788305")
        );
        assert!(
            rendered
                .contains("memeloop_token_center_request_spool_bytes{measure=\"limit\"} 134217728")
        );
        assert!(
            rendered.contains("memeloop_token_center_request_spool_capacity_rejections_total 3")
        );
    }

    #[test]
    fn memory_rejection_stages_render_exactly_six_fixed_series() {
        let metrics = Metrics::default();
        let series = |metrics: &Metrics| {
            metrics
                .render(&RuntimeMetrics::default())
                .lines()
                .filter(|line| {
                    line.starts_with("memeloop_token_center_proxy_memory_rejections_total{")
                })
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let labels = ["ingress", "json", "retained", "route", "plugin", "response"];
        assert_eq!(
            series(&metrics),
            labels.map(|stage| format!(
                "memeloop_token_center_proxy_memory_rejections_total{{stage=\"{stage}\"}} 0"
            ))
        );
        for (index, stage) in ProxyMemoryRejectionStage::ALL.into_iter().enumerate() {
            assert_eq!(stage.label(), labels[index]);
            for _ in 0..=index {
                let error =
                    metrics.observe_proxy_memory_error(stage, crate::error::AppError::Overloaded);
                assert!(matches!(error, crate::error::AppError::Overloaded));
            }
            let error = metrics.observe_proxy_memory_error(stage, crate::error::AppError::Internal);
            assert!(matches!(error, crate::error::AppError::Internal));
        }
        assert_eq!(
            series(&metrics),
            labels
                .into_iter()
                .enumerate()
                .map(|(index, stage)| format!(
                    "memeloop_token_center_proxy_memory_rejections_total{{stage=\"{stage}\"}} {}",
                    index + 1
                ))
                .collect::<Vec<_>>()
        );
    }
}
