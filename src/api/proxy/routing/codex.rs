use super::*;

pub(super) async fn send_proxy_route(
    state: &AppState,
    headers: &HeaderMap,
    request_id: Uuid,
    route: &PreparedProxyRoute,
) -> Result<(UpstreamResponse, crate::metrics::ActivityGuard), ProxySendError> {
    let outbound_base_url = codex_transport::outbound_base_url(&route.route.base_url);
    network::validate_codex_transport(
        &outbound_base_url,
        &route.route.config,
        route.route.credential.proxy(),
        state.config.allow_oauth_loopback,
    )
    .await
    .map_err(|_| ProxySendError::CandidateUnavailable)?;
    let target_url = network::upstream_api_url(&outbound_base_url, codex_transport::RESPONSES_PATH);
    let mut request = state
        .codex_http
        .post(target_url)
        .body(route.forwarded_body.clone());
    if let Some((proxy_url, _)) = route.route.credential.proxy() {
        let proxy =
            wreq::Proxy::all(proxy_url).map_err(|_| ProxySendError::CandidateUnavailable)?;
        request = request.proxy(proxy);
    }
    let session_id = route
        .codex_session_id
        .as_deref()
        .ok_or(ProxySendError::Credential)?;
    request = codex_transport::apply_wreq_wire_headers(
        request,
        headers,
        &route.route.credential,
        session_id,
    )
    .map_err(|_| ProxySendError::Credential)?;
    let upstream_activity = state.metrics.active_upstream(&route.route.driver, "proxy");
    let upstream_started = Instant::now();
    let upstream_result = request.send().await;
    state.metrics.observe_upstream(
        &route.route.driver,
        "proxy",
        upstream_result.as_ref().ok().map(wreq::Response::status),
        upstream_started.elapsed(),
    );
    match upstream_result {
        Ok(response) => {
            let response = UpstreamResponse::Codex(response);
            if response.status() == StatusCode::BAD_REQUEST {
                return match codex_transport::classify_bad_request(response).await {
                    codex_transport::BadRequestDisposition::Retryable => {
                        Err(ProxySendError::RetryableCodexBadRequest)
                    }
                    codex_transport::BadRequestDisposition::Ordinary => {
                        Err(ProxySendError::CodexBadRequest)
                    }
                };
            }
            if !response.status().is_success() {
                return Ok((response, upstream_activity));
            }
            let content_type_class = codex_transport::content_type_class(&response);
            let http_version = codex_transport::http_version_class(&response);
            match codex_transport::admit_event_stream_response(response).await {
                Ok(response) => Ok((response, upstream_activity)),
                Err(codex_transport::ResponseAdmissionError::Invalid(error_code)) => {
                    tracing::warn!(
                        %request_id,
                        upstream_account_id = %route.route.account_id,
                        content_type_class,
                        http_version,
                        stage = error_code,
                        "Codex upstream response failed framing admission"
                    );
                    Err(ProxySendError::InvalidResponse(error_code))
                }
                Err(codex_transport::ResponseAdmissionError::Ambiguous(error_code)) => {
                    tracing::warn!(
                        %request_id,
                        upstream_account_id = %route.route.account_id,
                        content_type_class,
                        http_version,
                        stage = error_code,
                        "Codex upstream response failed before framing admission completed"
                    );
                    Err(ProxySendError::AmbiguousResponse(error_code))
                }
            }
        }
        Err(error)
            if error.is_connect()
                || error.is_proxy_connect()
                || error.is_dns()
                || error.is_tls() =>
        {
            Err(ProxySendError::RetryableConnection)
        }
        Err(_) => Err(ProxySendError::NonRetryableTransport),
    }
}
