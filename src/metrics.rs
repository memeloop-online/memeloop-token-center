use std::{
    collections::BTreeMap,
    fmt::Write,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    time::{Duration, Instant},
};

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
    http: Mutex<BTreeMap<HttpLabels, RequestSeries>>,
    upstream: Mutex<BTreeMap<UpstreamLabels, RequestSeries>>,
    active_http_requests: AtomicI64,
    active_streams: [AtomicI64; ActiveStreamKind::COUNT],
    active_upstreams: Mutex<BTreeMap<UpstreamActivityLabels, i64>>,
    component_memory_bytes: [AtomicI64; MemoryComponent::COUNT],
    profiling: AtomicBool,
    database_ready: AtomicI64,
    archive_ready: AtomicI64,
    readiness: tokio::sync::Mutex<Option<CachedReadiness>>,
    process_started: Instant,
}

impl Default for MetricsInner {
    fn default() -> Self {
        Self {
            http: Mutex::default(),
            upstream: Mutex::default(),
            active_http_requests: AtomicI64::new(0),
            active_streams: std::array::from_fn(|_| AtomicI64::new(0)),
            active_upstreams: Mutex::default(),
            component_memory_bytes: std::array::from_fn(|_| AtomicI64::new(0)),
            profiling: AtomicBool::new(false),
            database_ready: AtomicI64::new(0),
            archive_ready: AtomicI64::new(0),
            readiness: tokio::sync::Mutex::default(),
            process_started: Instant::now(),
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
        let mut output = String::with_capacity(16 * 1024);

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
        render_active(&mut output, &self.inner);
        render_runtime(&mut output, runtime);
        render_process(&mut output, &self.inner);
        render_allocator(&mut output);
        output
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
        ("proxy_lifecycles", runtime.proxy_lifecycles),
        ("proxy_archive_streams", runtime.proxy_archive_streams),
    ] {
        let _ = writeln!(
            output,
            "memeloop_token_center_background_work_items{{queue=\"{queue}\",state=\"active\"}} {value}"
        );
    }
    output.push_str(
        "# HELP memeloop_token_center_plugin_cache_entries Resolved plugin configuration cache entries.\n",
    );
    output.push_str("# TYPE memeloop_token_center_plugin_cache_entries gauge\n");
    let _ = writeln!(
        output,
        "memeloop_token_center_plugin_cache_entries {}",
        runtime.plugin_cache_entries
    );
    output.push_str(
        "# HELP memeloop_token_center_plugin_cache_bytes Estimated bytes retained by the resolved plugin configuration cache.\n",
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
}
