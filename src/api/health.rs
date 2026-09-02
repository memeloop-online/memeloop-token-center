use super::*;

// The archive check has its own five-second deadline. Keep one second of
// headroom here so it can return a deliberate failure instead of being
// cancelled by the dependency aggregator first. The Helm probe adds one
// further second around this handler.
const CHECK_TIMEOUT: Duration = Duration::from_secs(6);

pub(super) async fn liveness() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

pub(super) async fn deprecated_health() -> Response {
    let mut response = liveness().await.into_response();
    response.headers_mut().insert(
        header::HeaderName::from_static("deprecation"),
        HeaderValue::from_static("true"),
    );
    response.headers_mut().insert(
        header::LINK,
        HeaderValue::from_static("</livez>; rel=\"successor-version\""),
    );
    response
}

pub(super) async fn readiness(State(state): State<AppState>) -> Response {
    let database = state.db.clone();
    let archive = state.archive.clone();
    let (database_ready, archive_ready) = state
        .metrics
        .readiness(move || async move {
            let (database, archive) = tokio::join!(
                tokio::time::timeout(CHECK_TIMEOUT, database.readiness_check()),
                tokio::time::timeout(CHECK_TIMEOUT, archive.readiness_check()),
            );
            let database_ready = matches!(database, Ok(Ok(())));
            let archive_ready = matches!(archive, Ok(Ok(())));
            if !database_ready {
                tracing::warn!(
                    timed_out = database.is_err(),
                    "readiness database check failed"
                );
            }
            if !archive_ready {
                tracing::warn!(
                    timed_out = archive.is_err(),
                    "readiness archive check failed"
                );
            }
            (database_ready, archive_ready)
        })
        .await;
    let status = if database_ready && archive_ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({
            "status": if status.is_success() { "ready" } else { "not_ready" },
            "checks": {
                "database": if database_ready { "ok" } else { "failed" },
                "archive": if archive_ready { "ok" } else { "failed" }
            }
        })),
    )
        .into_response()
}

pub(super) async fn prometheus_metrics(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    require_service(&headers, &state, "metrics:read").await?;
    let runtime = match state.db.runtime_metrics().await {
        Ok(value) => {
            state.metrics.set_dependency_ready("database", true);
            Some(value)
        }
        Err(error) => {
            state.metrics.set_dependency_ready("database", false);
            tracing::warn!(%error, "database runtime metrics collection failed");
            None
        }
    };
    let plugin = state.plugins.runtime_metrics().await;
    let runtime = crate::metrics::RuntimeMetrics {
        database: runtime,
        request_event_streams: state.request_event_streams.active_count(),
        proxy_lifecycles: (state.config.proxy_lifecycle_concurrency as usize)
            .saturating_sub(state.proxy_lifecycle_permits.available_permits()),
        proxy_archive_streams: crate::PROXY_ARCHIVE_STREAM_CONCURRENCY
            .saturating_sub(state.proxy_archive_stream_permits.available_permits()),
        plugin_cache_entries: plugin.cache_entries,
        plugin_cache_bytes: plugin.cache_bytes,
        loaded_plugins: plugin.loaded_plugins,
    };
    Ok((
        [
            (
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        state.metrics.render(&runtime),
    )
        .into_response())
}

pub(super) async fn version(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    require_service(&headers, &state, "metrics:read").await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
        "service": "memeloop-token-center",
        "version": crate::metrics::BUILD_VERSION,
        "revision": crate::metrics::BUILD_GIT_SHA,
        "build_timestamp": crate::metrics::BUILD_TIMESTAMP,
        "target": crate::metrics::BUILD_TARGET,
        "api": {
            "current": "v1",
            "supported": ["v1"],
            "compatibility": "additive changes may occur within v1; removals require a documented deprecation window",
            "deprecated": [{
                "path": "/healthz",
                "replacement": "/livez"
            }]
        }
        })),
    )
        .into_response())
}

pub(super) async fn observe_http(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let _active_request = state.metrics.active_http_request();
    let method = request.method().clone();
    // `MatchedPath` is a route template, never a concrete URI containing a key,
    // tenant, request id or other user-controlled high-cardinality value.
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str)
        .unwrap_or("unmatched")
        .to_owned();
    let started = Instant::now();
    let response = next.run(request).await;
    state
        .metrics
        .observe_http(&method, &route, response.status(), started.elapsed());
    response
}

pub(super) async fn security_headers(request: Request, next: Next) -> Response {
    let authenticated_api = matches!(
        request.uri().path().split('/').nth(1),
        Some("internal" | "self" | "v1")
    );
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    if authenticated_api {
        if !headers.contains_key(header::CACHE_CONTROL) {
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        }
        headers.insert(
            header::HeaderName::from_static("x-mtc-api-version"),
            HeaderValue::from_static("v1"),
        );
    }
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::STRICT_TRANSPORT_SECURITY,
        HeaderValue::from_static("max-age=31536000"),
    );
    headers.insert(
        header::HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static(
            "accelerometer=(), camera=(), geolocation=(), microphone=(), payment=(), usb=()",
        ),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_readiness_checks_have_a_bounded_six_second_deadline() {
        assert_eq!(CHECK_TIMEOUT, Duration::from_secs(6));
    }
}
