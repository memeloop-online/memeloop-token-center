use super::*;

// The database timeout is independent of configurable archive deadlines.
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

fn readiness_contract(database_ready: bool, archive_ready: bool) -> (StatusCode, Value) {
    // Kubernetes may safely send traffic while the durable database is
    // reachable. Archive availability is still surfaced as an explicit
    // degradation, but must not withdraw every gateway endpoint during an
    // object-store outage.
    let status = if database_ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let readiness = match (database_ready, archive_ready) {
        (true, true) => "ready",
        (true, false) => "degraded",
        (false, _) => "not_ready",
    };
    (
        status,
        json!({
            "status": readiness,
            "checks": {
                "database": if database_ready { "ok" } else { "failed" },
                "archive": if archive_ready { "ok" } else { "failed" }
            }
        }),
    )
}

pub(super) async fn readiness(State(state): State<AppState>) -> Response {
    let database = state.db.clone();
    let archive = state.archive.clone();
    let (database_ready, archive_ready) = state
        .metrics
        .readiness(move || async move {
            let (database, archive) = tokio::join!(
                tokio::time::timeout(CHECK_TIMEOUT, database.readiness_check()),
                tokio::time::timeout(
                    archive.readiness_deadline() + Duration::from_secs(1),
                    archive.readiness_check()
                ),
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
    let (status, body) = readiness_contract(database_ready, archive_ready);
    (status, Json(body)).into_response()
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
    let (
        proxy_memory_used_bytes,
        proxy_memory_limit_bytes,
        retained_request_memory_used_bytes,
        retained_request_memory_limit_bytes,
    ) = state.proxy_memory_budget.snapshot();
    let runtime = crate::metrics::RuntimeMetrics {
        database: runtime,
        request_event_streams: state.request_event_streams.active_count(),
        gateway_body_rejections: state.gateway_body_rejections.snapshot(),
        proxy_memory_used_bytes,
        proxy_memory_limit_bytes,
        retained_request_memory_used_bytes,
        retained_request_memory_limit_bytes,
        gateway_body_reads: (state.config.gateway_body_read_concurrency as usize)
            .saturating_sub(state.gateway_body_read_permits.available_permits()),
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
        state.metrics.render(&runtime) + &state.archive.readiness_metrics(),
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
    let response = if let Some(route_class) = proxy_diagnostics::route_class(request.uri().path()) {
        let context = proxy_diagnostics::Context::new();
        let ingress_request_id = proxy_diagnostics::ingress_request_id(
            request
                .headers()
                .get("x-request-id")
                .and_then(|value| value.to_str().ok()),
        );
        proxy_diagnostics::CONTEXT.scope(context, async move {
            tracing::info!(request_id = %context.request_id, ?ingress_request_id, route_class, phase = "gateway_entry", "proxy request entered gateway");
            let phase = proxy_diagnostics::Phase::new(context, "gateway_response_headers");
            let mut response = next.run(request).await;
            phase.finish("completed", Some(response.status().as_u16()), None);
            // The same server-owned ID is used by durable proxy admission.
            // Also return it on failures before a request record can exist.
            if let Ok(value) = HeaderValue::from_str(&context.request_id.to_string()) {
                response.headers_mut().insert(REQUEST_ID_HEADER, value);
            }
            proxy_diagnostics::observe_response(response, context)
        }).await
    } else if let Some(route_class) = proxy_diagnostics::control_route_class(&route) {
        let context = proxy_diagnostics::Context::new();
        let ingress_request_id = proxy_diagnostics::ingress_request_id(
            request
                .headers()
                .get("x-request-id")
                .and_then(|value| value.to_str().ok()),
        );
        proxy_diagnostics::CONTEXT
            .scope(context, async move {
                tracing::info!(request_id = %context.request_id, ?ingress_request_id, route_class,
                phase = "control_entry", "control request entered service");
                let phase = proxy_diagnostics::Phase::new(context, "control_response_headers");
                let mut response = next.run(request).await;
                phase.finish("completed", Some(response.status().as_u16()), None);
                if let Ok(value) = HeaderValue::from_str(&context.request_id.to_string()) {
                    response.headers_mut().insert(REQUEST_ID_HEADER, value);
                }
                response
            })
            .await
    } else {
        next.run(request).await
    };
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

    #[test]
    fn healthy_database_and_archive_report_ready() {
        let (status, body) = readiness_contract(true, true);

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ready");
        assert_eq!(body["checks"]["database"], "ok");
        assert_eq!(body["checks"]["archive"], "ok");
    }

    #[test]
    fn archive_failure_is_reported_as_degraded_without_withdrawing_readiness() {
        let (status, body) = readiness_contract(true, false);

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "degraded");
        assert_eq!(body["checks"]["database"], "ok");
        assert_eq!(body["checks"]["archive"], "failed");
    }

    #[test]
    fn database_failure_withdraws_readiness_even_when_archive_is_healthy() {
        let (status, body) = readiness_contract(false, true);

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["status"], "not_ready");
        assert_eq!(body["checks"]["database"], "failed");
        assert_eq!(body["checks"]["archive"], "ok");
    }

    #[test]
    fn simultaneous_database_and_archive_failure_remains_not_ready() {
        let (status, body) = readiness_contract(false, false);

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["status"], "not_ready");
        assert_eq!(body["checks"]["database"], "failed");
        assert_eq!(body["checks"]["archive"], "failed");
    }
}
