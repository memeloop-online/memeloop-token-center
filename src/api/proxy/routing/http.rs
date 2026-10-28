use super::*;
use bytes::Bytes;

const MAX_COMPACT_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy)]
struct RequestTimeout {
    duration: Option<std::time::Duration>,
    deadline: Option<tokio::time::Instant>,
}

fn log_preparation_failure(
    request_id: Uuid,
    account_id: Uuid,
    error: Option<&AppError>,
) -> &'static str {
    let failure_category = error
        .map(network::preparation_failure_category)
        .unwrap_or("preparation_timeout");
    tracing::warn!(
        %request_id,
        upstream_account_id = %account_id,
        stage = "http_json_preparation",
        failure_category,
        dispatched = false,
        "upstream preparation failed"
    );
    failure_category
}

fn record_preparation_failure(
    state: &AppState,
    request_id: Uuid,
    route: &ResolvedUpstream,
    error: Option<&AppError>,
) {
    let failure_category = log_preparation_failure(request_id, route.account_id, error);
    super::probe::record_preparation_failure(
        state,
        super::probe::PreparationFailure {
            request_id,
            route_id: route.route_id,
            upstream_account_id: route.account_id,
            credential_generation: route.credential_generation,
            transport_revision: route.transport_revision,
            failure_stage: failure_category,
        },
    );
}

/// `timeout_seconds` is a request-local transport policy. Read it once from
/// the immutable route snapshot so a later candidate cannot refresh an
/// already-dispatched attempt's deadline.
fn configured_timeout(config: &Value) -> Result<RequestTimeout, ProxySendError> {
    let seconds = match config.get("timeout_seconds") {
        None => {
            return Ok(RequestTimeout {
                duration: None,
                deadline: None,
            });
        }
        Some(value) => value
            .as_u64()
            .or_else(|| {
                value
                    .as_f64()
                    .filter(|seconds| {
                        seconds.fract() == 0.0 && *seconds > 0.0 && *seconds < u64::MAX as f64
                    })
                    .map(|seconds| seconds as u64)
            })
            .filter(|seconds| *seconds > 0)
            .ok_or(ProxySendError::CandidateUnavailable)?,
    };
    let duration = std::time::Duration::from_secs(seconds);
    let deadline = tokio::time::Instant::now()
        .checked_add(duration)
        .ok_or(ProxySendError::CandidateUnavailable)?;
    Ok(RequestTimeout {
        duration: Some(duration),
        deadline: Some(deadline),
    })
}

pub(super) async fn send_reqwest_proxy_route(
    state: &AppState,
    headers: &HeaderMap,
    protocol: Protocol,
    request_id: Uuid,
    route: &PreparedProxyRoute,
) -> Result<ProxyRouteResponse, ProxySendError> {
    let request_timeout = configured_timeout(&route.route.config)?;
    let managed =
        crate::oauth::authorization_code::supports_managed_transport(&route.route.credential);
    if !managed
        && route
            .route
            .config
            .get(crate::db::transport_proxy_management::CONFIG_KEY)
            .is_none()
    {
        return send_reqwest_proxy_route_once(
            state,
            headers,
            protocol,
            request_id,
            route,
            &route.route.credential,
            request_timeout,
        )
        .await;
    }
    let adapter = state
        .providers
        .get(&route.route.driver)
        .and_then(|provider| provider.oauth_adapter.as_ref())
        .ok_or(ProxySendError::CandidateUnavailable)?;
    crate::oauth::authorization_code::validate_installed_contribution(
        &route.route.credential,
        adapter,
    )
    .map_err(|_| ProxySendError::CandidateUnavailable)?;
    let selection = state
        .transport_proxy_groups
        .select_config(
            route.route.account_id,
            route.route.credential_generation,
            &route.route.credential,
            &route.route.config,
        )
        .map_err(|_| ProxySendError::CandidateUnavailable)?;
    let result = send_reqwest_proxy_route_once(
        state,
        headers,
        protocol,
        request_id,
        route,
        &selection.credential,
        request_timeout,
    )
    .await;
    if matches!(result, Err(ProxySendError::RetryableConnection(_))) {
        selection.request_independent_probe();
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn send_reqwest_proxy_route_once(
    state: &AppState,
    headers: &HeaderMap,
    protocol: Protocol,
    request_id: Uuid,
    route: &PreparedProxyRoute,
    transport_credential: &UpstreamCredential,
    request_timeout: RequestTimeout,
) -> Result<ProxyRouteResponse, ProxySendError> {
    send_reqwest_proxy_route_with_preparation(
        state,
        headers,
        protocol,
        request_id,
        route,
        request_timeout,
        network::client_for_config_url_no_retry(
            &state.http,
            &route.route.base_url,
            &route.route.config,
            transport_credential.proxy(),
            state.config.allow_oauth_loopback,
        ),
    )
    .await
}

async fn send_reqwest_proxy_route_with_preparation<F>(
    state: &AppState,
    headers: &HeaderMap,
    protocol: Protocol,
    request_id: Uuid,
    route: &PreparedProxyRoute,
    request_timeout: RequestTimeout,
    preparation: F,
) -> Result<ProxyRouteResponse, ProxySendError>
where
    F: std::future::Future<Output = Result<reqwest::Client, AppError>>,
{
    // Start the route timeout before endpoint validation/DNS. The deadline is
    // local to this attempt; the caller's frozen outer attempt budget remains
    // authoritative when it selects a standby candidate.
    let RequestTimeout {
        duration: timeout,
        deadline: request_deadline,
    } = request_timeout;
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
    let outbound_base_url = route.route.base_url.clone();
    let preparation_result = match request_deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, preparation).await,
        None => Ok(preparation.await),
    };
    let outbound_http = match preparation_result {
        Ok(Ok(client)) => client,
        // Endpoint validation and DNS happen before the POST leaves this
        // process, so a route-budget expiry here is safe to fail over.
        Err(_) => {
            let failure_stage = log_preparation_failure(request_id, route.route.account_id, None);
            return Err(ProxySendError::RetryableConnection(failure_stage));
        }
        Ok(Err(error)) => {
            record_preparation_failure(state, request_id, &route.route, Some(&error));
            return Err(ProxySendError::CandidateUnavailable);
        }
    };
    let target_url = network::upstream_api_url(&outbound_base_url, route.upstream_path);
    let mut request = outbound_http
        .post(target_url)
        .body(route.forwarded_body.clone());
    if let Some(timeout) = timeout {
        request = request.timeout(timeout);
    }
    let accept = if ((route.responses_chat.is_some() || route.responses_anthropic.is_some())
        && route.upstream_stream)
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
    if protocol.is_anthropic() || route.responses_anthropic.is_some() {
        let declared = state
            .providers
            .get(&route.route.driver)
            .map(|provider| {
                provider
                    .request_compatibility
                    .anthropic_messages_headers
                    .clone()
            })
            .unwrap_or_default();
        request = crate::api::anthropic::apply_declared_request_headers(
            request,
            headers,
            &declared,
            route.route.driver == crate::oauth::claude::PROVIDER_DRIVER,
        )
        .map_err(|_| ProxySendError::CandidateUnavailable)?;
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
            } else if let Some(context) = route.responses_anthropic.clone() {
                super::anthropic::translate(
                    response,
                    context,
                    route.upstream_stream,
                    sse_framing_limits,
                )?
            } else {
                UpstreamResponse::Reqwest(response)
            };
            Ok(ProxyRouteResponse {
                egress_selection: None,
                response: match (request_deadline, timeout) {
                    (Some(deadline), Some(timeout)) => {
                        response.with_body_timeouts(deadline, timeout)
                    }
                    _ => response,
                },
                upstream_activity,
                codex_retry: CodexRetryTerminalGuard::inactive(),
                sse_framing_limits,
            })
        }
        Err(error) => Err(error),
    }
}

async fn send_until_request_deadline<F>(
    deadline: Option<tokio::time::Instant>,
    send: F,
) -> Result<reqwest::Response, ProxySendError>
where
    F: std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    let Some(deadline) = deadline else {
        return send.await.map_err(classify_reqwest_send_error);
    };
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
    let bytes = read_compact_body(response.bytes_stream(), MAX_COMPACT_RESPONSE_BYTES).await?;
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

async fn read_compact_body<Error>(
    stream: impl futures_util::Stream<Item = Result<Bytes, Error>>,
    limit: usize,
) -> Result<Bytes, ProxySendError> {
    tokio::pin!(stream);
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|_| ProxySendError::AmbiguousResponse("upstream_invalid_response"))?;
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(ProxySendError::AmbiguousResponse(
                "upstream_invalid_response",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn generic_group_connection_failure_keeps_the_sticky_exit() {
        let account_id = Uuid::now_v7();
        let groups = crate::db::TransportProxyGroups::parse(
            &json!([{
                "account_id":account_id, "version":1,
                "proxies":["socks5h://10.20.30.40:1080", "socks5h://10.20.30.41:1080"]
            }])
            .to_string(),
            b"generic-http-fixture-key",
        )
        .unwrap();
        let credential: UpstreamCredential = serde_json::from_value(json!({
            "type":"oauth", "access_token":"fixture", "proxy_url":"socks5h://10.20.30.40:1080", "proxy_network_scope":"private"
        })).unwrap();
        let selected = groups.select(account_id, 1, &credential).unwrap();
        selected.request_independent_probe();
        assert_eq!(
            groups.select(account_id, 1, &credential).unwrap().member(),
            Some(0)
        );
        assert!(groups.advance_sticky_after_probe(account_id));
        assert_eq!(
            groups.select(account_id, 1, &credential).unwrap().member(),
            Some(1)
        );
        assert!(groups.advance_sticky_after_probe(account_id));
        assert_eq!(
            groups.select(account_id, 1, &credential).unwrap().member(),
            Some(0)
        );
    }

    fn compact_response(status: StatusCode, body: &'static str) -> reqwest::Response {
        http::Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(reqwest::Body::from(body))
            .unwrap()
            .into()
    }

    #[tokio::test]
    async fn compact_translation_preserves_upstream_error_status_headers_and_body() {
        let response = translate_new_api_compact_v2(
            compact_response(StatusCode::TOO_MANY_REQUESTS, "private-canary error"),
            true,
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        let mut stream = response.bytes_stream();
        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            Bytes::from_static(b"private-canary error")
        );
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn compact_translation_never_returns_completed_for_failed_or_incomplete_body() {
        for body in [
            r#"{"status":"failed","output":[{"type":"compaction","encrypted_content":"opaque"}]}"#,
            r#"{"status":"incomplete","output":[{"type":"compaction","encrypted_content":"opaque"}]}"#,
        ] {
            for wrap_as_sse in [false, true] {
                let result = translate_new_api_compact_v2(
                    compact_response(StatusCode::OK, body),
                    wrap_as_sse,
                )
                .await;
                assert!(matches!(
                    result,
                    Err(ProxySendError::AmbiguousResponse(
                        "upstream_invalid_response"
                    ))
                ));
            }
        }
    }

    #[tokio::test]
    async fn compact_body_limit_accepts_exact_boundary_and_empty_chunks() {
        let stream = futures_util::stream::iter([
            Ok::<_, &'static str>(Bytes::from_static(b"ab")),
            Ok(Bytes::new()),
            Ok(Bytes::from_static(b"cd")),
        ]);
        assert_eq!(
            read_compact_body(stream, 4).await.unwrap(),
            Bytes::from_static(b"abcd")
        );
    }

    #[tokio::test]
    async fn compact_body_limit_stops_on_first_overflow_chunk() {
        for chunks in [
            vec![b"abcde".as_slice()],
            vec![b"ab".as_slice(), b"cde".as_slice()],
        ] {
            let stream = futures_util::stream::iter(
                chunks
                    .into_iter()
                    .map(|chunk| Ok::<_, &'static str>(Bytes::from_static(chunk))),
            )
            .chain(futures_util::stream::poll_fn(|_| {
                panic!("oversized compact body must stop before polling again")
            }));
            assert!(matches!(
                read_compact_body(stream, 4).await,
                Err(ProxySendError::AmbiguousResponse(
                    "upstream_invalid_response"
                ))
            ));
        }
    }

    #[tokio::test]
    async fn compact_body_read_failure_preserves_ambiguous_nonreplayable_error() {
        let stream =
            futures_util::stream::iter([Ok(Bytes::from_static(b"partial")), Err("private-canary")]);
        assert!(matches!(
            read_compact_body(stream, 64).await,
            Err(ProxySendError::AmbiguousResponse(
                "upstream_invalid_response"
            ))
        ));
    }

    #[test]
    fn preparation_logs_only_fixed_categories_and_dispatch_state() {
        use std::sync::{Arc, Mutex};
        #[derive(Clone)]
        struct Writer(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = Writer(captured.clone());
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_writer(move || sink.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            log_preparation_failure(
                Uuid::nil(),
                Uuid::nil(),
                Some(&AppError::BadRequest(
                    "https://user:secret@example.test/?token=secret".into(),
                )),
            );
            log_preparation_failure(Uuid::nil(), Uuid::nil(), None);
        });
        let output = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
        assert!(!output.contains("secret"));
        assert!(!output.contains("example.test"));
        let events: Vec<Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0]["fields"]["failure_category"],
            "destination_rejected"
        );
        assert_eq!(
            events[1]["fields"]["failure_category"],
            "preparation_timeout"
        );
        for event in events {
            assert_eq!(event["fields"]["dispatched"], false);
            assert_eq!(event["fields"]["stage"], "http_json_preparation");
        }
    }

    #[test]
    fn configured_timeout_preserves_explicit_seconds_and_rejects_invalid_values() {
        assert_eq!(configured_timeout(&json!({})).unwrap().duration, None);
        for seconds in [1, 120, 600, 601, 900, 1261] {
            assert_eq!(
                configured_timeout(&json!({"timeout_seconds": seconds}))
                    .unwrap()
                    .duration,
                Some(Duration::from_secs(seconds))
            );
        }
        assert_eq!(
            configured_timeout(&json!({"timeout_seconds": 900.0}))
                .unwrap()
                .duration,
            Some(Duration::from_secs(900))
        );
        for invalid in [
            json!(0),
            json!(-1),
            json!(1.5),
            json!(null),
            json!("900"),
            json!(u64::MAX),
        ] {
            assert!(matches!(
                configured_timeout(&json!({"timeout_seconds": invalid})),
                Err(ProxySendError::CandidateUnavailable)
            ));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn default_send_waits_for_real_result_after_old_deadlines() {
        let timeout = configured_timeout(&json!({})).unwrap();
        let started = tokio::time::Instant::now();
        let response = send_until_request_deadline(timeout.deadline, async {
            tokio::time::sleep(Duration::from_secs(1_300)).await;
            Ok(compact_response(StatusCode::OK, "{}"))
        })
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(started.elapsed(), Duration::from_secs(1_300));
    }

    #[tokio::test(start_paused = true)]
    async fn explicit_timeout_remains_frozen_across_preparation_and_elapsed_stream_time() {
        let request_timeout = configured_timeout(&json!({"timeout_seconds": 900})).unwrap();
        let started = tokio::time::Instant::now();
        tokio::time::advance(Duration::from_secs(601)).await;
        assert_eq!(request_timeout.duration, Some(Duration::from_secs(900)));
        assert_eq!(
            request_timeout.deadline,
            Some(started + Duration::from_secs(900))
        );
        let response = send_until_request_deadline(request_timeout.deadline, async {
            tokio::time::sleep(Duration::from_secs(250)).await;
            Ok(compact_response(StatusCode::OK, "{}"))
        })
        .await;
        assert!(response.is_ok());
        let result =
            send_until_request_deadline(request_timeout.deadline, std::future::pending()).await;
        assert!(matches!(
            result,
            Err(ProxySendError::NonRetryableTransport(
                TransportFailureKind::Timeout
            ))
        ));
        assert_eq!(started.elapsed(), Duration::from_secs(900));
    }

    #[tokio::test(start_paused = true)]
    async fn request_deadline_timeout_is_ambiguous_and_never_replayable() {
        let deadline = Some(tokio::time::Instant::now() + Duration::from_secs(1));
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

    #[tokio::test(start_paused = true)]
    async fn explicit_900_second_request_remains_cancellable_past_600_seconds() {
        let request_timeout = configured_timeout(&json!({"timeout_seconds": 900})).unwrap();
        let (started, entered) = tokio::sync::oneshot::channel::<()>();
        let (dropped, observed) = tokio::sync::oneshot::channel::<()>();
        let send = tokio::spawn(async move {
            send_until_request_deadline(request_timeout.deadline, async move {
                let _dropped = dropped;
                started.send(()).unwrap();
                std::future::pending().await
            })
            .await
        });
        entered.await.unwrap();
        tokio::time::advance(Duration::from_secs(601)).await;
        assert!(!send.is_finished());
        send.abort();
        assert!(send.await.unwrap_err().is_cancelled());
        assert!(observed.await.is_err());
    }

    async fn assert_preparation_timeout_has_one_terminal_writer(drain_before_terminal: bool) {
        let (state, _directory) = crate::api::tests::test_state().await;
        let request_id = Uuid::now_v7();
        let account_id = Uuid::now_v7();
        let route = PreparedProxyRoute {
            route: ResolvedUpstream {
                route_id: Uuid::now_v7(),
                account_id,
                transport_revision: 42,
                credential_generation: 7,
                driver: "http-json".into(),
                base_url: "https://preparation.invalid".into(),
                config: json!({"timeout_seconds": 1}),
                upstream_model: "preparation-test".into(),
                credential: UpstreamCredential::None,
            },
            forwarded_body: Bytes::from_static(b"{}"),
            upstream_stream: false,
            codex_downstream_stream: false,
            codex_store_disabled: false,
            codex_session_id: None,
            component_request: None,
            responses_chat: None,
            upstream_path: "/v1/chat/completions",
            wait_for_compaction: false,
            compact_v2_bridge: false,
            wrap_compact_as_sse: false,
            responses_anthropic: None,
        };
        let pool = sqlx::AnyPool::connect(&state.config.database_url)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO upstream_accounts (
                 id, tenant_id, name, driver, auth_kind, config_json, status,
                 credential_generation, created_at, updated_at
             ) VALUES ($1, $2, 'preparation-test', 'http-json', 'none', '{}', 'active', 7, 0, 42)",
        )
        .bind(account_id.to_string())
        .bind(Uuid::now_v7().to_string())
        .execute(&pool)
        .await
        .unwrap();
        let admission = state
            .db
            .claim_upstream_account_attempt_at_revision_with_health_config(
                account_id,
                7,
                42,
                state.config.upstream_health,
                Some(current_gateway_failure_domain()),
            )
            .await
            .unwrap();
        assert!(admission.is_healthy());
        let mut guard = UpstreamAttemptGuard::new(
            &state,
            request_id,
            route.route.route_id,
            account_id,
            7,
            42,
            admission,
            None,
        );
        let result = send_reqwest_proxy_route_with_preparation(
            &state,
            &HeaderMap::new(),
            Protocol::OpenAiChat,
            request_id,
            &route,
            configured_timeout(&route.route.config).unwrap(),
            std::future::pending(),
        )
        .await;
        assert!(matches!(
            result,
            Err(ProxySendError::RetryableConnection("preparation_timeout"))
        ));
        assert_eq!(
            failover_disposition(None, result.as_ref().err()),
            super::super::outcome::FailoverDisposition::ConnectionNotDelivered
        );
        if drain_before_terminal {
            state.routing_persistence.drain_for_test().await;
            assert!(
                state
                    .db
                    .request_upstream_transport_diagnostics(request_id)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        let (kind, reason) = classify_attempt_failure(&result, None).unwrap();
        assert_eq!(kind, UpstreamFailureKind::Connection);
        assert!(matches!(
            reason,
            crate::metrics::UpstreamHealthReason::Connection
        ));
        guard
            .complete(UpstreamAttemptTerminal::Failed {
                kind,
                reason,
                failure_stage: attempt_failure_stage(&result),
            })
            .await;
        state.routing_persistence.drain_for_test().await;
        let diagnostics = state
            .db
            .request_upstream_transport_diagnostics(request_id)
            .await
            .unwrap();
        assert_eq!(diagnostics.len(), 1);
        let diagnostic = &diagnostics[0];
        assert_eq!(diagnostic.route_id, route.route.route_id);
        assert_eq!(diagnostic.upstream_account_id, account_id);
        assert_eq!(diagnostic.credential_generation, 7);
        assert_eq!(diagnostic.transport_revision, 42);
        assert_eq!(diagnostic.failure_kind, "preparation");
        assert_eq!(diagnostic.failure_stage, "preparation_timeout");
        assert!(
            state.routing_persistence.render_routing().contains(
                "memeloop_token_center_routing_persistence_total{outcome=\"accepted\"} 1\n"
            )
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn preparation_timeout_immediate_terminal_preserves_diagnostic() {
        assert_preparation_timeout_has_one_terminal_writer(false).await;
    }

    #[tokio::test]
    async fn preparation_timeout_delayed_terminal_has_no_preparation_writer() {
        assert_preparation_timeout_has_one_terminal_writer(true).await;
    }
}
