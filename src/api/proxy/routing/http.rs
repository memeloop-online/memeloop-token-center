use super::*;

pub(super) async fn send_reqwest_proxy_route(
    state: &AppState,
    headers: &HeaderMap,
    protocol: Protocol,
    request_id: Uuid,
    route: &PreparedProxyRoute,
) -> Result<ProxyRouteResponse, ProxySendError> {
    let outbound_base_url = route.route.base_url.clone();
    let outbound_http = network::client_for_config_url(
        &state.http,
        &outbound_base_url,
        &route.route.config,
        route.route.credential.proxy(),
        state.config.allow_oauth_loopback,
    )
    .await
    .map_err(|_| ProxySendError::CandidateUnavailable)?;
    let target_url = network::upstream_api_url(&outbound_base_url, protocol.path());
    let mut request = outbound_http
        .post(target_url)
        .body(route.forwarded_body.clone());
    // For a CBCNX Responses call that has opted into streaming, pin the
    // upstream representation to SSE instead of inheriting a downstream
    // `Accept: application/json` default. Keep generic compatible routes
    // transparent.
    let accept = if route.route.driver == crate::provider::CBCNX_PROVIDER_DRIVER
        && matches!(protocol, Protocol::OpenAiResponses)
        && route.upstream_stream
    {
        HeaderValue::from_static("text/event-stream")
    } else {
        headers
            .get(header::ACCEPT)
            .cloned()
            .unwrap_or(HeaderValue::from_static("application/json"))
    };
    request = request
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, accept);
    let credential_now = credential_application_now();
    request = route
        .route
        .credential
        .apply(request, credential_now)
        .map_err(|_| credential_application_error(&route.route.credential, credential_now))?;
    if route.route.driver == crate::oauth::copilot::PROVIDER_DRIVER {
        let product = format!("memeloop-token-center/{}", env!("CARGO_PKG_VERSION"));
        request = request
            .header(header::USER_AGENT, &product)
            .header("X-GitHub-Api-Version", "2026-06-01")
            .header("X-Request-Id", request_id.to_string())
            .header("Editor-Version", &product)
            .header("Editor-Plugin-Version", &product);
    }
    if let Some(version) = headers.get("anthropic-version") {
        request = request.header("anthropic-version", version);
    }
    if route.route.driver == crate::oauth::claude::PROVIDER_DRIVER {
        request = request.header("anthropic-beta", crate::oauth::claude::OAUTH_BETA_HEADER);
    } else if let Some(beta) = headers.get("anthropic-beta") {
        request = request.header("anthropic-beta", beta);
    }
    let upstream_activity = state.metrics.active_upstream(&route.route.driver, "proxy");
    let upstream_started = Instant::now();
    let upstream_result = request.send().await;
    state.metrics.observe_upstream(
        &route.route.driver,
        "proxy",
        upstream_result.as_ref().ok().map(reqwest::Response::status),
        upstream_started.elapsed(),
    );
    match upstream_result {
        Ok(response) => Ok(ProxyRouteResponse {
            response: UpstreamResponse::Reqwest(response),
            upstream_activity,
            codex_retry: CodexRetryTerminalGuard::inactive(),
        }),
        Err(error) if error.is_connect() => Err(ProxySendError::RetryableConnection),
        // Do not replay ambiguous POST delivery.
        Err(_) => Err(ProxySendError::NonRetryableTransport),
    }
}
