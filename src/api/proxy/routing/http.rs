use super::*;
use bytes::Bytes;

const DEFAULT_TIMEOUT_SECONDS: u64 = 120;
const MAX_TIMEOUT_SECONDS: u64 = 600;

/// `timeout_seconds` is a request-local transport policy. Read it once from
/// the immutable route snapshot so a later candidate cannot refresh an
/// already-dispatched attempt's deadline. Persisted configs are schema
/// validated, but keep the runtime fallback bounded for older/manual rows.
fn configured_timeout(config: &Value) -> std::time::Duration {
    std::time::Duration::from_secs(
        config
            .get("timeout_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_SECONDS)
            .clamp(1, MAX_TIMEOUT_SECONDS),
    )
}

pub(super) async fn send_reqwest_proxy_route(
    state: &AppState,
    headers: &HeaderMap,
    protocol: Protocol,
    request_id: Uuid,
    route: &PreparedProxyRoute,
) -> Result<ProxyRouteResponse, ProxySendError> {
    // Start the route timeout before endpoint validation/DNS. The deadline is
    // local to this attempt; the caller's frozen outer attempt budget remains
    // authoritative when it selects a standby candidate.
    let timeout = configured_timeout(&route.route.config);
    let sse_framing_limits = crate::provider::SseFramingLimits::default();
    tracing::info!(
        %request_id,
        upstream_account_id = %route.route.account_id,
        transport_policy_source = "global_default",
        transport_policy_version = 1,
        max_sse_event_bytes = sse_framing_limits.event_bytes,
        max_sse_framed_bytes = sse_framing_limits.framed_bytes,
        max_sse_terminal_hold_bytes = sse_framing_limits.terminal_hold_bytes,
        stage = "sse_framing_policy_snapshot",
        "upstream SSE framing policy frozen"
    );
    let request_deadline = tokio::time::Instant::now() + timeout;
    let outbound_base_url = route.route.base_url.clone();
    let outbound_http = match tokio::time::timeout_at(
        request_deadline,
        network::client_for_config_url_no_retry(
            &state.http,
            &outbound_base_url,
            &route.route.config,
            route.route.credential.proxy(),
            state.config.allow_oauth_loopback,
        ),
    )
    .await
    {
        Ok(Ok(client)) => client,
        // Endpoint validation and DNS happen before the POST leaves this
        // process, so a route-budget expiry here is safe to fail over.
        Err(_) => return Err(ProxySendError::RetryableConnection("dns")),
        Ok(Err(_)) => return Err(ProxySendError::CandidateUnavailable),
    };
    let target_url = network::upstream_api_url(&outbound_base_url, route.upstream_path);
    let mut request = outbound_http
        .post(target_url)
        // Reqwest's per-request timeout covers connection/header acquisition;
        // the absolute deadline below also covers DNS/client setup and body
        // reads, so no phase can restart the configured attempt budget.
        .timeout(timeout)
        .body(route.forwarded_body.clone());
    // Streaming Responses on HTTP JSON (including retired CBCNX rows) must
    // negotiate SSE even when the client sent `Accept: application/json`.
    let accept = if (route.responses_chat.is_some() && route.upstream_stream)
        || (crate::provider::is_openai_compatible_http_driver(&route.route.driver)
            && matches!(protocol, Protocol::OpenAiResponses)
            && route.upstream_stream)
        || (crate::provider::is_new_api_driver(&route.route.driver)
            && matches!(protocol, Protocol::OpenAiResponses)
            && route.upstream_stream)
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
    if route.route.driver == crate::oauth::managed::kimi::PROVIDER_DRIVER {
        request = crate::oauth::managed::kimi::apply_headers(request, &route.route.credential)
            .map_err(|_| ProxySendError::Credential)?;
    }
    if route.route.driver == crate::oauth::copilot::PROVIDER_DRIVER {
        let product = format!("memeloop-token-center/{}", env!("CARGO_PKG_VERSION"));
        request = request
            .header(header::USER_AGENT, &product)
            .header("X-GitHub-Api-Version", "2026-06-01")
            .header("X-Request-Id", request_id.to_string())
            .header("Editor-Version", &product)
            .header("Editor-Plugin-Version", &product);
    }
    if protocol.is_anthropic() {
        request = crate::api::anthropic::apply_request_headers(
            request,
            headers,
            route.route.driver == crate::oauth::claude::PROVIDER_DRIVER,
        );
    }
    let upstream_activity = state.metrics.active_upstream(&route.route.driver, "proxy");
    let upstream_started = Instant::now();
    let upstream_result = send_until_request_deadline(request_deadline, request.send()).await;
    state.metrics.observe_upstream(
        &route.route.driver,
        "proxy",
        upstream_result.as_ref().ok().map(reqwest::Response::status),
        upstream_started.elapsed(),
    );
    match upstream_result {
        Ok(response) => {
            let response = if let Some(context) = route.responses_chat.clone() {
                super::kimi::translate(
                    response,
                    context,
                    route.upstream_stream,
                    sse_framing_limits,
                )?
            } else if route.compact_v2_bridge {
                translate_new_api_compact_v2(response, route.wrap_compact_as_sse).await?
            } else {
                UpstreamResponse::Reqwest(response)
            };
            Ok(ProxyRouteResponse {
                // Keep one absolute deadline from before DNS through first
                // byte and every subsequent streaming read. The inactivity
                // window uses the same configured scalar and resets only on
                // actual upstream progress.
                response: response.with_body_timeouts(request_deadline, timeout),
                upstream_activity,
                codex_retry: CodexRetryTerminalGuard::inactive(),
                sse_framing_limits,
            })
        }
        Err(error) => Err(error),
    }
}

async fn send_until_request_deadline<F>(
    deadline: tokio::time::Instant,
    send: F,
) -> Result<reqwest::Response, ProxySendError>
where
    F: std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    tokio::pin!(send);
    tokio::select! {
        // A POST may have been accepted before the local deadline fired;
        // preserve the ambiguous-delivery fence and never replay it.
        biased;
        _ = tokio::time::sleep_until(deadline) => Err(ProxySendError::NonRetryableTransport(
            TransportFailureKind::Timeout,
        )),
        result = &mut send => result.map_err(classify_reqwest_send_error),
    }
}

fn classify_reqwest_send_error(error: reqwest::Error) -> ProxySendError {
    if error.is_connect() {
        return ProxySendError::RetryableConnection("connect");
    }
    // Do not replay ambiguous POST delivery. Persist only allowlisted error
    // classes; the error may contain the upstream URL and must not escape.
    let kind = if error.is_timeout() {
        TransportFailureKind::Timeout
    } else if error.is_body() {
        TransportFailureKind::Body
    } else if error.is_decode() {
        TransportFailureKind::Decode
    } else if error.is_request() {
        TransportFailureKind::Request
    } else {
        TransportFailureKind::Other
    };
    ProxySendError::NonRetryableTransport(kind)
}

async fn translate_new_api_compact_v2(
    response: reqwest::Response,
    wrap_as_sse: bool,
) -> Result<UpstreamResponse, ProxySendError> {
    let status = response.status();
    let version = response.version();
    let mut headers = response.headers().clone();
    let bytes = response
        .bytes()
        .await
        .map_err(|_| ProxySendError::AmbiguousResponse("upstream_invalid_response"))?;
    if bytes.len() > 64 * 1024 * 1024 {
        return Err(ProxySendError::AmbiguousResponse(
            "upstream_invalid_response",
        ));
    }
    if !status.is_success() {
        let stream: crate::api::proxy::upstream_response::UpstreamByteStream =
            Box::pin(futures_util::stream::once(async move { Ok(bytes) }));
        return Ok(UpstreamResponse::Prefetched {
            status,
            headers,
            version,
            content_length: None,
            stream,
        });
    }
    let value = crate::api::sse::parse_unique_json(&bytes)
        .map_err(|_| ProxySendError::AmbiguousResponse("upstream_invalid_response"))?;
    let responses = crate::api::new_api_transport::compact_to_responses(&value)
        .map_err(|_| ProxySendError::AmbiguousResponse("upstream_invalid_response"))?;
    let (content_type, body) = if wrap_as_sse {
        (
            HeaderValue::from_static("text/event-stream"),
            crate::api::new_api_transport::responses_to_sse(&responses)
                .map_err(|_| ProxySendError::AmbiguousResponse("upstream_invalid_response"))?,
        )
    } else {
        (
            HeaderValue::from_static("application/json"),
            Bytes::from(
                serde_json::to_vec(&responses)
                    .map_err(|_| ProxySendError::AmbiguousResponse("upstream_invalid_response"))?,
            ),
        )
    };
    headers.remove(header::CONTENT_LENGTH);
    headers.remove(header::CONTENT_ENCODING);
    headers.insert(header::CONTENT_TYPE, content_type);
    let stream: crate::api::proxy::upstream_response::UpstreamByteStream =
        Box::pin(futures_util::stream::once(async move { Ok(body) }));
    Ok(UpstreamResponse::Prefetched {
        status,
        headers,
        version,
        content_length: None,
        stream,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn configured_timeout_obeys_the_provider_schema_bounds() {
        assert_eq!(configured_timeout(&json!({})), Duration::from_secs(120));
        assert_eq!(
            configured_timeout(&json!({"timeout_seconds": 1})),
            Duration::from_secs(1)
        );
        assert_eq!(
            configured_timeout(&json!({"timeout_seconds": 600})),
            Duration::from_secs(600)
        );
        assert_eq!(
            configured_timeout(&json!({"timeout_seconds": 0})),
            Duration::from_secs(1)
        );
        assert_eq!(
            configured_timeout(&json!({"timeout_seconds": 601})),
            Duration::from_secs(600)
        );
        assert_eq!(
            configured_timeout(&json!({"timeout_seconds": -1})),
            Duration::from_secs(120)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn request_deadline_timeout_is_ambiguous_and_never_replayable() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let result = send_until_request_deadline(deadline, std::future::pending()).await;
        assert!(matches!(
            result,
            Err(ProxySendError::NonRetryableTransport(
                TransportFailureKind::Timeout
            ))
        ));

        tokio::time::advance(Duration::from_secs(1)).await;
        let result = send_until_request_deadline(deadline, async {
            std::future::pending::<Result<reqwest::Response, reqwest::Error>>().await
        })
        .await;
        assert!(matches!(
            result,
            Err(ProxySendError::NonRetryableTransport(
                TransportFailureKind::Timeout
            ))
        ));
    }
}
