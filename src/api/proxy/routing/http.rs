use super::*;
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

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
    let duration = crate::api::traffic::configured_http_timeout(config)
        .map_err(|_| ProxySendError::CandidateUnavailable)?;
    let Some(duration) = duration else {
        return Ok(RequestTimeout {
            duration: None,
            deadline: None,
        });
    };
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

#[cfg(test)]
tokio::task_local! {
    static STALL_PROBE_PREPARATION: bool;
}

#[cfg(test)]
pub(in crate::api::proxy) async fn with_stalled_probe_preparation<F: std::future::Future>(
    future: F,
) -> F::Output {
    STALL_PROBE_PREPARATION.scope(true, future).await
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
        async {
            #[cfg(test)]
            if STALL_PROBE_PREPARATION
                .try_with(|stall| *stall)
                .unwrap_or(false)
            {
                std::future::pending::<()>().await;
            }
            network::client_for_config_url_no_retry(
                &state.http,
                &route.route.base_url,
                &route.route.config,
                transport_credential.proxy(),
                state.config.allow_oauth_loopback,
            )
            .await
        },
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
    let upstream_result = send_until_request_deadline(request_deadline, async {
        let (client, request) = request.build_split();
        let request = request?;
        IndependentProbeTarget::dispatched(&route.route);
        let response = client.execute(request).await?;
        IndependentProbeTarget::response(response.status());
        Ok(response)
    })
    .await;
    state.metrics.observe_upstream(
        &route.route.driver,
        "proxy",
        upstream_result.as_ref().ok().map(reqwest::Response::status),
        upstream_started.elapsed(),
    );
    match upstream_result {
        Ok(response) => {
            let response = if route.compact_v2_bridge {
                translate_new_api_compact_v2(
                    response,
                    route.wrap_compact_as_sse,
                    std::path::Path::new(&state.config.responses_request_spool_path),
                )
                .await?
            } else {
                UpstreamResponse::Reqwest(response)
            };
            let response = match (request_deadline, timeout) {
                (Some(deadline), Some(timeout)) => response.with_body_timeouts(deadline, timeout),
                _ => response,
            };
            let (response, preoutput_quota) =
                if route.inspect_stream_rejection && response.validated_compact().is_none() {
                    super::classify_stream_quota(
                        response,
                        sse_framing_limits,
                        &state.proxy_memory_budget,
                    )
                    .await
                } else {
                    (response, None)
                };
            let response = if preoutput_quota.is_some() {
                response
            } else if let Some(context) = route.responses_chat.clone() {
                super::kimi::translate(
                    response,
                    context,
                    route.upstream_stream,
                    sse_framing_limits,
                )?
            } else if let Some(context) = route.responses_anthropic.clone() {
                super::anthropic::translate(
                    response,
                    context,
                    route.upstream_stream,
                    sse_framing_limits,
                )?
            } else {
                response
            };
            Ok(ProxyRouteResponse {
                preoutput_quota,
                egress_selection: None,
                response,
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
    spool_path: &std::path::Path,
) -> Result<UpstreamResponse, ProxySendError> {
    let status = response.status();
    // Only successful compact payloads require translation. Preserve an
    // upstream rejection as a live response so ordinary error handling owns
    // its body, accounting, and delivery without waiting for EOF here.
    if !status.is_success() {
        return Ok(UpstreamResponse::Reqwest(response));
    }
    let version = response.version();
    let mut headers = response.headers().clone();
    let file = capture_compact_body(response.bytes_stream(), spool_path).await?;
    let output = new_compact_file(spool_path).await?;
    let index = new_compact_file(spool_path).await?.into_std().await;
    let output = output.into_std().await;
    let file = file.into_std().await;
    struct CancelTranslation(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for CancelTranslation {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    let cancellation = CancelTranslation(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
        false,
    )));
    let cancelled = cancellation.0.clone();
    let output = tokio::task::spawn_blocking(move || {
        super::compact_file::translate_cancellable(file, output, index, wrap_as_sse, cancelled)
    })
    .await
    .map_err(|_| ProxySendError::AmbiguousResponse("upstream_invalid_response"))?
    .map_err(|error| {
        ProxySendError::AmbiguousResponse(if error == "compact_file_io" {
            "upstream_compact_spool_write_failed"
        } else {
            "upstream_invalid_response"
        })
    })?;
    let evidence = output.evidence;
    let pieces = output.pieces;
    let physical_body_bytes = output.physical_body_bytes;
    let output = output.file;
    let content_type = if wrap_as_sse {
        HeaderValue::from_static("text/event-stream")
    } else {
        HeaderValue::from_static("application/json")
    };
    headers.remove(header::CONTENT_LENGTH);
    headers.remove(header::CONTENT_ENCODING);
    headers.insert(header::CONTENT_TYPE, content_type);
    let stream = compact_file_stream(
        output,
        pieces,
        physical_body_bytes,
        evidence.expected_body_bytes(),
    );
    Ok(UpstreamResponse::ValidatedCompact {
        status,
        headers,
        version,
        content_length: None,
        stream,
        evidence,
    })
}

fn compact_file_stream(
    output: std::fs::File,
    pieces: std::fs::File,
    physical_bytes: u64,
    expected_bytes: u64,
) -> crate::api::proxy::upstream_response::UpstreamByteStream {
    let source: crate::api::proxy::upstream_response::UpstreamByteStream =
        Box::pin(futures_util::stream::try_unfold(
            (
                tokio::fs::File::from_std(output),
                tokio::fs::File::from_std(pieces),
                None::<std::ops::Range<u64>>,
            ),
            move |(mut file, mut plan, mut active)| async move {
                loop {
                    if let Some(range) = active.as_mut() {
                        let length = (range.end - range.start).min(64 * 1024) as usize;
                        let mut bytes = vec![0; length];
                        file.read_exact(&mut bytes)
                            .await
                            .map_err(|_| "upstream_compact_spool_read_failed")?;
                        range.start += length as u64;
                        if range.start == range.end {
                            active = None;
                        }
                        return Ok(Some((Bytes::from(bytes), (file, plan, active))));
                    }
                    let mut tag = [0];
                    if plan
                        .read(&mut tag)
                        .await
                        .map_err(|_| "upstream_compact_spool_read_failed")?
                        == 0
                    {
                        file.seek(std::io::SeekFrom::Start(physical_bytes))
                            .await
                            .map_err(|_| "upstream_compact_spool_read_failed")?;
                        if file
                            .read(&mut [0; 1])
                            .await
                            .map_err(|_| "upstream_compact_spool_read_failed")?
                            != 0
                        {
                            return Err("upstream_compact_spool_read_failed");
                        }
                        return Ok(None);
                    }
                    let first = plan
                        .read_u64_le()
                        .await
                        .map_err(|_| "upstream_compact_spool_read_failed")?;
                    match tag[0] {
                        0 => {
                            let end = plan
                                .read_u64_le()
                                .await
                                .map_err(|_| "upstream_compact_spool_read_failed")?;
                            if first > end || end > physical_bytes {
                                return Err("upstream_compact_spool_read_failed");
                            }
                            if first == end {
                                continue;
                            }
                            file.seek(std::io::SeekFrom::Start(first))
                                .await
                                .map_err(|_| "upstream_compact_spool_read_failed")?;
                            active = Some(first..end);
                        }
                        1 => {
                            // Literals are generated protocol punctuation and event names,
                            // never provider data. Their record has a fixed representation.
                            let mut literal = [0; 256];
                            let length = usize::try_from(first)
                                .map_err(|_| "upstream_compact_spool_read_failed")?;
                            let bytes = literal
                                .get_mut(..length)
                                .ok_or("upstream_compact_spool_read_failed")?;
                            plan.read_exact(bytes)
                                .await
                                .map_err(|_| "upstream_compact_spool_read_failed")?;
                            if bytes.is_empty() {
                                continue;
                            }
                            return Ok(Some((Bytes::copy_from_slice(bytes), (file, plan, active))));
                        }
                        _ => return Err("upstream_compact_spool_read_failed"),
                    }
                }
            },
        ));
    // Retain one chunk until the next read confirms progress or exact EOF. In
    // particular a completed SSE event cannot escape before its file tail has
    // been verified. These are chunk buffers, not whole-event buffers.
    Box::pin(futures_util::stream::try_unfold(
        (source, None::<Bytes>, 0u64, false),
        move |(mut source, mut pending, mut read_bytes, finished)| async move {
            if finished {
                return Ok(None);
            }
            loop {
                let Some(next) = source.next().await else {
                    if read_bytes != expected_bytes {
                        return Err("upstream_compact_spool_read_failed");
                    }
                    return Ok(pending
                        .take()
                        .map(|chunk| (chunk, (source, None, read_bytes, true))));
                };
                let next = next?;
                read_bytes = read_bytes
                    .checked_add(next.len() as u64)
                    .filter(|count| *count <= expected_bytes)
                    .ok_or("upstream_compact_spool_read_failed")?;
                if let Some(chunk) = pending.replace(next) {
                    return Ok(Some((chunk, (source, pending, read_bytes, false))));
                }
            }
        },
    ))
}

async fn new_compact_file(spool_path: &std::path::Path) -> Result<tokio::fs::File, ProxySendError> {
    tokio::fs::create_dir_all(spool_path)
        .await
        .map_err(|_| ProxySendError::AmbiguousResponse("upstream_compact_spool_unavailable"))?;
    let directory = spool_path.to_owned();
    let file = tokio::task::spawn_blocking(move || tempfile::tempfile_in(directory))
        .await
        .map_err(|_| ProxySendError::AmbiguousResponse("upstream_compact_spool_unavailable"))?
        .map_err(|_| ProxySendError::AmbiguousResponse("upstream_compact_spool_unavailable"))?;
    Ok(tokio::fs::File::from_std(file))
}

async fn capture_compact_body<Error>(
    stream: impl futures_util::Stream<Item = Result<Bytes, Error>>,
    spool_path: &std::path::Path,
) -> Result<tokio::fs::File, ProxySendError> {
    tokio::pin!(stream);
    let mut file = new_compact_file(spool_path).await?;
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|_| ProxySendError::AmbiguousResponse("upstream_invalid_response"))?;
        file.write_all(&chunk).await.map_err(|_| {
            ProxySendError::AmbiguousResponse("upstream_compact_spool_write_failed")
        })?;
    }
    file.flush()
        .await
        .map_err(|_| ProxySendError::AmbiguousResponse("upstream_compact_spool_write_failed"))?;
    Ok(file)
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
        let directory = tempfile::tempdir().unwrap();
        let response = translate_new_api_compact_v2(
            compact_response(StatusCode::TOO_MANY_REQUESTS, "private-canary error"),
            true,
            directory.path(),
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
    async fn compact_rejection_returns_before_body_is_polled() {
        let directory = tempfile::tempdir().unwrap();
        for status in [StatusCode::TOO_MANY_REQUESTS, StatusCode::BAD_GATEWAY] {
            let body = futures_util::stream::poll_fn(
                |_| -> std::task::Poll<Option<Result<Bytes, std::io::Error>>> {
                    panic!("compact translation must not poll an upstream rejection body")
                },
            );
            let response = http::Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "text/plain")
                .header(header::RETRY_AFTER, "7")
                .body(reqwest::Body::wrap_stream(body))
                .unwrap()
                .into();
            let response = translate_new_api_compact_v2(response, true, directory.path())
                .await
                .unwrap();
            assert_eq!(response.status(), status);
            assert_eq!(response.headers()[header::CONTENT_TYPE], "text/plain");
            assert_eq!(response.headers()[header::RETRY_AFTER], "7");
            assert!(matches!(response, UpstreamResponse::Reqwest(_)));
        }
    }

    #[tokio::test]
    async fn compact_translation_never_returns_completed_for_failed_or_incomplete_body() {
        let directory = tempfile::tempdir().unwrap();
        for body in [
            r#"{"status":"failed","output":[{"type":"compaction","encrypted_content":"opaque"}]}"#,
            r#"{"status":"incomplete","output":[{"type":"compaction","encrypted_content":"opaque"}]}"#,
        ] {
            for wrap_as_sse in [false, true] {
                let result = translate_new_api_compact_v2(
                    compact_response(StatusCode::OK, body),
                    wrap_as_sse,
                    directory.path(),
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
    async fn compact_file_validation_evidence_survives_body_timeout_wrapper() {
        let directory = tempfile::tempdir().unwrap();
        let response = translate_new_api_compact_v2(
            compact_response(
                StatusCode::OK,
                r#"{"id":"response-compact","usage":{"input_tokens":11,"output_tokens":3},"output":[{"type":"compaction","encrypted_content":"opaque"}]}"#,
            ),
            true,
            directory.path(),
        )
        .await
        .unwrap();
        let evidence = response.validated_compact().expect("host evidence");
        let length = evidence.expected_body_bytes();
        assert_eq!(evidence.summary().usage.unwrap().input_tokens, 11);
        let response = response.with_body_timeouts(
            tokio::time::Instant::now() + Duration::from_secs(60),
            Duration::from_secs(60),
        );
        let evidence = response.validated_compact().expect("preserved evidence");
        assert_eq!(evidence.expected_body_bytes(), length);
        assert_eq!(evidence.summary().usage.unwrap().output_tokens, 3);
        let mut stream = response.bytes_stream();
        let mut received = 0u64;
        while let Some(chunk) = stream.next().await {
            received += chunk.unwrap().len() as u64;
        }
        assert_eq!(received, length);
    }

    #[tokio::test]
    async fn compact_body_capture_preserves_chunks_and_empty_chunks() {
        let directory = tempfile::tempdir().unwrap();
        let stream = futures_util::stream::iter([
            Ok::<_, &'static str>(Bytes::from_static(b"ab")),
            Ok(Bytes::new()),
            Ok(Bytes::from_static(b"cd")),
        ]);
        let mut file = capture_compact_body(stream, directory.path())
            .await
            .unwrap();
        tokio::io::AsyncSeekExt::rewind(&mut file).await.unwrap();
        let mut body = Vec::new();
        file.read_to_end(&mut body).await.unwrap();
        assert_eq!(body, b"abcd");
    }

    #[tokio::test]
    async fn compact_body_capture_has_no_old_64_mib_cutoff() {
        let directory = tempfile::tempdir().unwrap();
        let chunk = Bytes::from(vec![b'x'; 64 * 1024]);
        let count = 1025usize;
        let stream =
            futures_util::stream::iter((0..count).map(|_| Ok::<_, &'static str>(chunk.clone())));
        let file = capture_compact_body(stream, directory.path())
            .await
            .unwrap();
        assert_eq!(
            file.metadata().await.unwrap().len(),
            (count * chunk.len()) as u64
        );
    }

    #[tokio::test]
    async fn compact_body_read_failure_preserves_ambiguous_nonreplayable_error() {
        let directory = tempfile::tempdir().unwrap();
        let stream =
            futures_util::stream::iter([Ok(Bytes::from_static(b"partial")), Err("private-canary")]);
        assert!(matches!(
            capture_compact_body(stream, directory.path()).await,
            Err(ProxySendError::AmbiguousResponse(
                "upstream_invalid_response"
            ))
        ));
    }

    #[tokio::test]
    async fn compact_file_tail_requires_exact_eof_before_last_chunk() {
        use std::io::{Seek as _, Write as _};
        for (body, expected, expected_error) in [
            (b"complete".as_slice(), 8u64, false),
            (b"short".as_slice(), 8, true),
            (b"unexpected-tail".as_slice(), 8, true),
        ] {
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(body).unwrap();
            file.rewind().unwrap();
            let mut plan = tempfile::tempfile().unwrap();
            plan.write_all(&[0]).unwrap();
            plan.write_all(&0u64.to_le_bytes()).unwrap();
            plan.write_all(&expected.to_le_bytes()).unwrap();
            plan.rewind().unwrap();
            let mut stream = compact_file_stream(file, plan, expected, expected);
            let first = stream.next().await.unwrap();
            if expected_error {
                assert_eq!(first.unwrap_err(), "upstream_compact_spool_read_failed");
            } else {
                assert_eq!(first.unwrap(), body);
                assert!(stream.next().await.is_none());
            }
        }
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
            inspect_stream_rejection: false,
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
            failover_disposition(None, result.as_ref().err(), None),
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
