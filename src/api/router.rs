use super::*;

pub fn router(state: AppState) -> Router {
    router_for_role(state, RuntimeRole::All)
}

pub fn router_for_role(state: AppState, role: RuntimeRole) -> Router {
    let request_id_header = header::HeaderName::from_static(REQUEST_ID_HEADER);
    let mut application = Router::new()
        .route("/healthz", get(deprecated_health))
        .route("/livez", get(liveness))
        .route("/readyz", get(readiness));
    if matches!(role, RuntimeRole::Control | RuntimeRole::All) {
        application = application
            .route("/metrics", get(prometheus_metrics))
            .route("/version", get(version));
    }
    application = application.route("/ui-assets/{*path}", get(web_asset));
    if role.serves_control() {
        application = application.merge(control_router(state.clone()));
    }
    if role.serves_gateway() {
        application = application.merge(gateway_router(state.clone()));
    }
    application
        .layer(DefaultBodyLimit::max(MAX_DEFAULT_REQUEST_BODY))
        .layer(middleware::from_fn(security_headers))
        .layer(PropagateRequestIdLayer::new(request_id_header.clone()))
        .layer(SetRequestIdLayer::new(request_id_header, MakeRequestUuid))
        // Never attach the raw URI to tracing spans: paths and query strings
        // are attacker-controlled and commonly carry API keys or signed URLs.
        // Matched route templates are already recorded by `observe_http` after
        // routing; the request span only needs the non-secret method.
        .layer(TraceLayer::new_for_http().make_span_with(
            |request: &Request| tracing::info_span!("http_request", method = %request.method()),
        ))
        .layer(middleware::from_fn_with_state(state.clone(), observe_http))
        .with_state(state)
}
