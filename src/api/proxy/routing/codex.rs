use super::*;

use crate::metrics::{CodexBadRequestClassification, CodexBadRequestRetry};

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
    let session_id = route
        .codex_session_id
        .as_deref()
        .ok_or(ProxySendError::Credential)?;
    // `prepare_request_with_id` must have already forced this exact outbound
    // request to be non-persistent. Keep the retry guard explicit here so a
    // future request-preparation change cannot silently make an uncertain
    // operation replayable.
    let can_retry_definite_bad_request = route.codex_store_disabled;
    let mut retried_definite_bad_request = false;
    loop {
        let (response, upstream_activity) =
            match send_codex_attempt(state, headers, &target_url, route, session_id).await {
                Ok(response) => response,
                Err(error) => {
                    if retried_definite_bad_request {
                        state
                            .metrics
                            .observe_codex_bad_request_retry(CodexBadRequestRetry::Exhausted);
                    }
                    return Err(error);
                }
            };
        let response = UpstreamResponse::Codex(response);
        if response.status() == StatusCode::BAD_REQUEST {
            match codex_transport::classify_bad_request(response).await {
                codex_transport::BadRequestDisposition::Retryable => {
                    state.metrics.observe_codex_bad_request_classification(
                        CodexBadRequestClassification::Retryable,
                    );
                    if !retried_definite_bad_request && can_retry_definite_bad_request {
                        // An allowlisted, completely received HTTP 400 is the
                        // only native Codex response we treat as a definite
                        // pre-delivery rejection. Rebuild the request from the
                        // immutable PreparedProxyRoute so the wire body,
                        // headers, session identity, and store=false contract
                        // remain identical. No SSE has been admitted and no
                        // downstream bytes can have been sent on this path.
                        retried_definite_bad_request = true;
                        state
                            .metrics
                            .observe_codex_bad_request_retry(CodexBadRequestRetry::Started);
                        drop(upstream_activity);
                        continue;
                    }
                    if retried_definite_bad_request {
                        state
                            .metrics
                            .observe_codex_bad_request_retry(CodexBadRequestRetry::Exhausted);
                    }
                    return Err(ProxySendError::RetryableCodexBadRequest);
                }
                codex_transport::BadRequestDisposition::Ordinary(classification) => {
                    state
                        .metrics
                        .observe_codex_bad_request_classification(classification);
                    if classification == CodexBadRequestClassification::Ordinary
                        && !retried_definite_bad_request
                        && can_retry_definite_bad_request
                    {
                        // The full, bounded, single-JSON 400 was received
                        // before any downstream bytes. Send the immutable
                        // non-persistent wire request once more to this same
                        // account; malformed, oversized, timed-out, and
                        // content-type-ambiguous errors never take this path.
                        retried_definite_bad_request = true;
                        state
                            .metrics
                            .observe_codex_bad_request_retry(CodexBadRequestRetry::Started);
                        drop(upstream_activity);
                        continue;
                    }
                    if retried_definite_bad_request {
                        state
                            .metrics
                            .observe_codex_bad_request_retry(CodexBadRequestRetry::Exhausted);
                    }
                    return Err(ProxySendError::CodexBadRequest);
                }
            }
        }
        if !response.status().is_success() {
            if retried_definite_bad_request {
                state
                    .metrics
                    .observe_codex_bad_request_retry(CodexBadRequestRetry::Exhausted);
            }
            return Ok((response, upstream_activity));
        }
        let content_type_class = codex_transport::content_type_class(&response);
        let http_version = codex_transport::http_version_class(&response);
        match codex_transport::admit_event_stream_response(response).await {
            Ok(response) => {
                if retried_definite_bad_request {
                    state
                        .metrics
                        .observe_codex_bad_request_retry(CodexBadRequestRetry::Succeeded);
                }
                return Ok((response, upstream_activity));
            }
            Err(codex_transport::ResponseAdmissionError::Invalid(error_code)) => {
                if retried_definite_bad_request {
                    state
                        .metrics
                        .observe_codex_bad_request_retry(CodexBadRequestRetry::Exhausted);
                }
                tracing::warn!(
                    %request_id,
                    upstream_account_id = %route.route.account_id,
                    content_type_class,
                    http_version,
                    stage = error_code,
                    "Codex upstream response failed framing admission"
                );
                return Err(ProxySendError::InvalidResponse(error_code));
            }
            Err(codex_transport::ResponseAdmissionError::Ambiguous(error_code)) => {
                if retried_definite_bad_request {
                    state
                        .metrics
                        .observe_codex_bad_request_retry(CodexBadRequestRetry::Exhausted);
                }
                tracing::warn!(
                    %request_id,
                    upstream_account_id = %route.route.account_id,
                    content_type_class,
                    http_version,
                    stage = error_code,
                    "Codex upstream response failed before framing admission completed"
                );
                return Err(ProxySendError::AmbiguousResponse(error_code));
            }
        }
    }
}

async fn send_codex_attempt(
    state: &AppState,
    headers: &HeaderMap,
    target_url: &str,
    route: &PreparedProxyRoute,
    session_id: &str,
) -> Result<(wreq::Response, crate::metrics::ActivityGuard), ProxySendError> {
    let mut request = state
        .codex_http
        .post(target_url)
        .body(route.forwarded_body.clone());
    if let Some((proxy_url, _)) = route.route.credential.proxy() {
        let proxy =
            wreq::Proxy::all(proxy_url).map_err(|_| ProxySendError::CandidateUnavailable)?;
        request = request.proxy(proxy);
    }
    let request = codex_transport::apply_wreq_wire_headers(
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
        Ok(response) => Ok((response, upstream_activity)),
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
