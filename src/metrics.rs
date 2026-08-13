use std::{
    collections::BTreeMap,
    fmt::Write,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
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

#[derive(Default)]
struct MetricsInner {
    http: Mutex<BTreeMap<HttpLabels, RequestSeries>>,
    upstream: Mutex<BTreeMap<UpstreamLabels, RequestSeries>>,
    database_ready: AtomicI64,
    archive_ready: AtomicI64,
    readiness: tokio::sync::Mutex<Option<CachedReadiness>>,
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
            operation,
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

    pub fn render(&self, runtime: Option<&DatabaseRuntimeMetrics>) -> String {
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
        render_database_runtime(&mut output, runtime);
        output
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DatabaseRuntimeMetrics {
    pub pool_size: u32,
    pub pool_idle: usize,
    pub queued_jobs: i64,
    pub running_jobs: i64,
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

fn render_database_runtime(output: &mut String, runtime: Option<&DatabaseRuntimeMetrics>) {
    output.push_str(
        "# HELP memeloop_token_center_db_pool_connections Current SQL pool connections.\n",
    );
    output.push_str("# TYPE memeloop_token_center_db_pool_connections gauge\n");
    output.push_str("# HELP memeloop_token_center_generation_jobs Current asynchronous generation jobs by active status.\n");
    output.push_str("# TYPE memeloop_token_center_generation_jobs gauge\n");
    if let Some(runtime) = runtime {
        let in_use = usize::try_from(runtime.pool_size)
            .unwrap_or(usize::MAX)
            .saturating_sub(runtime.pool_idle);
        let _ = writeln!(
            output,
            "memeloop_token_center_db_pool_connections{{state=\"idle\"}} {}",
            runtime.pool_idle
        );
        let _ = writeln!(
            output,
            "memeloop_token_center_db_pool_connections{{state=\"in_use\"}} {in_use}"
        );
        let _ = writeln!(
            output,
            "memeloop_token_center_generation_jobs{{status=\"queued\"}} {}",
            runtime.queued_jobs
        );
        let _ = writeln!(
            output,
            "memeloop_token_center_generation_jobs{{status=\"running\"}} {}",
            runtime.running_jobs
        );
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
        "cpa-subscription-bridge" => "cpa-subscription-bridge",
        "comfyui" => "comfyui",
        "volcengine-seedance" => "volcengine-seedance",
        "legacy" => "legacy",
        value if value.starts_with("plugin:") => "plugin",
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
        let rendered = metrics.render(None);
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
        let rendered = metrics.render(None);
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
}
