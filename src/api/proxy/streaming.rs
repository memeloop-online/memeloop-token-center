use super::*;

mod archive;

use archive::{cancel_stream_archive, stream_response_archive};

pub(super) struct StreamingResponse<'a> {
    pub(super) state: &'a AppState,
    pub(super) upstream: UpstreamResponse,
    pub(super) status: StatusCode,
    pub(super) content_type: Option<HeaderValue>,
    pub(super) is_sse: bool,
    pub(super) capture_json_usage: bool,
    pub(super) protocol: Protocol,
    pub(super) is_codex_route: bool,
    pub(super) upstream_activity: crate::metrics::ActivityGuard,
    pub(super) request_id: Uuid,
    pub(super) buffered_request: BufferedRequest<'a>,
    pub(super) proxy_lifecycle_permit: tokio::sync::OwnedSemaphorePermit,
}

pub(super) async fn stream_response(input: StreamingResponse<'_>) -> Result<Response, AppError> {
    let StreamingResponse {
        state,
        upstream,
        status,
        content_type,
        is_sse,
        capture_json_usage,
        protocol,
        is_codex_route,
        upstream_activity,
        request_id,
        buffered_request,
        proxy_lifecycle_permit,
    } = input;
    // Archive capacity is advisory for text traffic. Never wait for it before
    // constructing the downstream response or reading the first upstream byte.
    let archive_stream_permit = buffered_request.archive_available.then(|| {
        state
            .proxy_archive_stream_permits
            .clone()
            .try_acquire_owned()
            .ok()
    });
    let archive_stream_permit = archive_stream_permit.flatten();
    if buffered_request.archive_available && archive_stream_permit.is_none() {
        tracing::warn!(%request_id, stage = "response_archive_capacity", "proxy archive gap");
    }
    let stream_activity = state
        .metrics
        .active_stream(crate::metrics::ActiveStreamKind::ProxyResponse);
    let (body_sender, body_receiver) = tokio::sync::mpsc::channel(PROXY_BODY_CHANNEL_CAPACITY);
    let background_state = state.clone();
    let status_code = i64::from(status.as_u16());
    let BufferedRequest {
        reservation,
        started,
        input_token_ceiling,
        output_token_ceiling,
        requested_service_tier,
        conversation,
        tenant_id,
        ..
    } = buffered_request;
    tokio::spawn(async move {
        // Streaming responses outlive the handler response. Keep the workload
        // permit inside this task until archive and billing finalization end.
        let _proxy_lifecycle_permit = proxy_lifecycle_permit;
        let _stream_activity = stream_activity;
        let _upstream_activity = upstream_activity;
        let lifecycle_started = tokio::time::Instant::now();
        let stream_deadline = lifecycle_started + MAX_PROXY_STREAM_LIFETIME;
        let lifecycle_deadline = lifecycle_started + MAX_PROXY_LIFETIME;
        let lifecycle = async move {
            let mut upstream_stream = upstream.bytes_stream();
            let archive_complete = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            let (mut archive_sender, archive_task) = if let Some(permit) = archive_stream_permit {
                let (sender, receiver) =
                    tokio::sync::mpsc::channel::<Bytes>(PROXY_BODY_CHANNEL_CAPACITY);
                let task = tokio::spawn(stream_response_archive(
                    background_state.clone(),
                    request_id,
                    permit,
                    receiver,
                    archive_complete.clone(),
                ));
                (Some(sender), Some(task))
            } else {
                (None, None)
            };
            let mut usage_capture = Vec::new();
            let mut capture_memory = background_state
                .metrics
                .memory_usage(crate::metrics::MemoryComponent::StreamCapture, 0);
            let mut sse_capture = is_sse.then(|| {
                if matches!(protocol, Protocol::OpenAiResponses) {
                    ResponsesSseCapture::for_responses()
                } else {
                    ResponsesSseCapture::default()
                }
            });
            let mut responses_streaming_sanitizer = (is_sse
                && matches!(protocol, Protocol::OpenAiResponses))
            .then(codex_transport::ResponsesStreamingSanitizer::default);
            let mut transport_error: Option<&'static str> = None;
            let mut response_bytes = 0_usize;
            let mut delivered_any = false;
            let mut delivered_billable = false;
            loop {
                let next =
                    match tokio::time::timeout_at(stream_deadline, upstream_stream.next()).await {
                        Ok(next) => next,
                        Err(_) => {
                            transport_error = Some("upstream_timeout");
                            cancel_stream_archive(&archive_complete, &mut archive_sender);
                            let _ = tokio::time::timeout(
                                MAX_DOWNSTREAM_SEND_WAIT,
                                body_sender
                                    .send(Err(std::io::Error::other("upstream stream timed out"))),
                            )
                            .await;
                            break;
                        }
                    };
                let Some(next) = next else {
                    if responses_streaming_sanitizer
                        .as_ref()
                        .is_some_and(|sanitizer| !sanitizer.is_complete())
                    {
                        transport_error = Some("upstream_incomplete_response");
                    }
                    break;
                };
                match next {
                    Ok(raw_chunk) => {
                        let _response_buffer = background_state.metrics.memory_usage(
                            crate::metrics::MemoryComponent::ResponseBuffer,
                            raw_chunk.len(),
                        );
                        response_bytes = response_bytes.saturating_add(raw_chunk.len());
                        if response_bytes > MAX_PROXY_RESPONSE_BODY {
                            transport_error = Some("upstream_response_too_large");
                            cancel_stream_archive(&archive_complete, &mut archive_sender);
                            let _ = tokio::time::timeout(
                                MAX_DOWNSTREAM_SEND_WAIT,
                                body_sender.send(Err(std::io::Error::other(
                                    "upstream response exceeded the size limit",
                                ))),
                            )
                            .await;
                            break;
                        }
                        let (chunk, chunk_billable) = if let Some(sanitizer) =
                            responses_streaming_sanitizer.as_mut()
                        {
                            match sanitizer.push(&raw_chunk) {
                                Ok(chunk) => {
                                    let billable = if is_codex_route {
                                        sanitizer.last_push_billable()
                                    } else {
                                        // Generic compatible Responses routes historically
                                        // charge the admitted contract once any response event
                                        // is delivered, including a redacted terminal failure.
                                        // Direct Codex routes retain their narrower lifecycle
                                        // classification because their trusted transport can
                                        // distinguish non-billable control events.
                                        !chunk.is_empty()
                                    };
                                    (chunk, billable)
                                }
                                Err(error_code) => {
                                    transport_error = Some(error_code);
                                    cancel_stream_archive(&archive_complete, &mut archive_sender);
                                    let _ = tokio::time::timeout(
                                        MAX_DOWNSTREAM_SEND_WAIT,
                                        body_sender.send(Err(std::io::Error::other(
                                            "upstream stream violated the Responses protocol",
                                        ))),
                                    )
                                    .await;
                                    break;
                                }
                            }
                        } else {
                            (raw_chunk, true)
                        };
                        if capture_json_usage {
                            append_bounded(&mut usage_capture, &chunk, 2 * 1024 * 1024);
                            capture_memory.set_bytes(usage_capture.capacity());
                        }
                        if let Some(capture) = sse_capture.as_mut() {
                            capture.push(&chunk);
                        }
                        if let Some(sender) = archive_sender.as_ref()
                            && sender.try_send(chunk.clone()).is_err()
                        {
                            tracing::warn!(%request_id, stage = "response_archive_backpressure", "proxy archive gap");
                            cancel_stream_archive(&archive_complete, &mut archive_sender);
                        }
                        if chunk.is_empty() {
                            continue;
                        }
                        if !delivered_any {
                            match tokio::time::timeout(
                                MAX_DOWNSTREAM_SEND_WAIT,
                                body_sender.reserve(),
                            )
                            .await
                            {
                                Ok(Ok(permit)) => {
                                    match prepare_proxy_delivery_with_retry(
                                        &background_state.db,
                                        request_id,
                                        tenant_id,
                                        &reservation,
                                        input_token_ceiling,
                                        output_token_ceiling,
                                        requested_service_tier.as_deref(),
                                    )
                                    .await
                                    {
                                        Ok(()) => {
                                            let initial_length =
                                                chunk.len().min(MAX_UNCONFIRMED_DELIVERY_BYTES);
                                            let initial = chunk.slice(..initial_length);
                                            let remaining = chunk.slice(initial_length..);
                                            permit.send(Ok::<Bytes, std::io::Error>(initial));
                                            record_delivered_chunk(
                                                &mut delivered_any,
                                                &mut delivered_billable,
                                                chunk_billable,
                                            );
                                            if confirm_proxy_delivery_with_retry(
                                                &background_state.db,
                                                request_id,
                                                tenant_id,
                                                &reservation,
                                            )
                                            .await
                                            .is_err()
                                            {
                                                transport_error = Some("delivery_state");
                                            } else if !remaining.is_empty() {
                                                match tokio::time::timeout(
                                                    MAX_DOWNSTREAM_SEND_WAIT,
                                                    body_sender.send(Ok::<Bytes, std::io::Error>(
                                                        remaining,
                                                    )),
                                                )
                                                .await
                                                {
                                                    Ok(Ok(())) => {}
                                                    Ok(Err(_)) => {
                                                        transport_error =
                                                            Some("downstream_disconnected")
                                                    }
                                                    Err(_) => {
                                                        transport_error =
                                                            Some("downstream_backpressure")
                                                    }
                                                }
                                            }
                                        }
                                        Err(_) => {
                                            drop(permit);
                                            transport_error = Some("delivery_state");
                                            let _ = tokio::time::timeout(
                                                MAX_DOWNSTREAM_SEND_WAIT,
                                                body_sender.send(Err(std::io::Error::other(
                                                    "response delivery could not be recorded",
                                                ))),
                                            )
                                            .await;
                                        }
                                    }
                                }
                                Ok(Err(_)) => transport_error = Some("downstream_disconnected"),
                                Err(_) => transport_error = Some("downstream_backpressure"),
                            }
                        } else {
                            match tokio::time::timeout(
                                MAX_DOWNSTREAM_SEND_WAIT,
                                body_sender.send(Ok::<Bytes, std::io::Error>(chunk)),
                            )
                            .await
                            {
                                Ok(Ok(())) => record_delivered_chunk(
                                    &mut delivered_any,
                                    &mut delivered_billable,
                                    chunk_billable,
                                ),
                                Ok(Err(_)) => transport_error = Some("downstream_disconnected"),
                                Err(_) => transport_error = Some("downstream_backpressure"),
                            }
                        }
                        if transport_error.is_some() {
                            cancel_stream_archive(&archive_complete, &mut archive_sender);
                            break;
                        }
                    }
                    Err(_) => {
                        transport_error = Some("upstream_stream");
                        cancel_stream_archive(&archive_complete, &mut archive_sender);
                        let _ = tokio::time::timeout(
                            MAX_DOWNSTREAM_SEND_WAIT,
                            body_sender.send(Err(std::io::Error::other("upstream stream failed"))),
                        )
                        .await;
                        break;
                    }
                }
            }
            if transport_error.is_some() {
                cancel_stream_archive(&archive_complete, &mut archive_sender);
            }
            // EOF is part of downstream delivery. Close it before awaiting the
            // archive sidecar or terminal settlement so neither can prolong
            // the client-visible stream lifetime.
            drop(body_sender);
            drop(archive_sender.take());
            let sse_summary = sse_capture.map(ResponsesSseCapture::finish_summary);
            let gap_response = format!("gap://{request_id}/response");
            let (response_archive_attempt, stored_response) = match archive_task {
                Some(task) => match task.await {
                    Ok(result) => result,
                    Err(_) => {
                        tracing::warn!(%request_id, stage = "response_archive_task", "proxy archive gap");
                        (None, gap_response.clone())
                    }
                },
                None => (None, gap_response.clone()),
            };
            let protocol_error = match sse_summary.as_ref().map(|summary| &summary.outcome) {
                Some(ResponsesSseOutcome::Failed) => Some("upstream_failed_response"),
                Some(ResponsesSseOutcome::Incomplete) => Some("upstream_incomplete_response"),
                Some(ResponsesSseOutcome::Completed { .. }) | None => None,
            };
            let mut terminal_status = status_code;
            let mut error_code = transport_error.or(protocol_error);
            if error_code.is_some() {
                terminal_status = 502;
            }
            let full_contract_usage = || TokenUsage {
                input_tokens: input_token_ceiling,
                output_tokens: output_token_ceiling,
                ..TokenUsage::default()
            };
            let mut charge_contract_ceiling = delivered_billable && error_code.is_some();
            let mut usage = if error_code.is_some() {
                if delivered_billable {
                    full_contract_usage()
                } else {
                    TokenUsage::default()
                }
            } else {
                let extracted_usage = match sse_summary.as_ref() {
                    Some(summary) if summary.usage_invalid => ExtractedUsage::Invalid,
                    Some(summary) => summary.usage.clone().map_or_else(
                        || {
                            if is_codex_route {
                                ExtractedUsage::Invalid
                            } else {
                                ExtractedUsage::Missing
                            }
                        },
                        ExtractedUsage::Valid,
                    ),
                    None => extract_usage_checked(&usage_capture),
                };
                match extracted_usage {
                    ExtractedUsage::Valid(usage) => usage,
                    ExtractedUsage::Missing => {
                        charge_contract_ceiling = delivered_billable;
                        if delivered_billable {
                            full_contract_usage()
                        } else {
                            TokenUsage::default()
                        }
                    }
                    ExtractedUsage::Invalid => {
                        terminal_status = 502;
                        error_code = Some("upstream_invalid_usage");
                        charge_contract_ceiling = delivered_billable;
                        if delivered_billable {
                            full_contract_usage()
                        } else {
                            TokenUsage::default()
                        }
                    }
                }
            };
            match crate::db::normalize_proxy_usage(
                &usage,
                input_token_ceiling,
                output_token_ceiling,
                requested_service_tier.as_deref(),
            ) {
                Ok(normalized) => usage = normalized,
                Err(AppError::Upstream(_)) => {
                    terminal_status = 502;
                    error_code = Some("upstream_invalid_usage");
                    charge_contract_ceiling = delivered_billable;
                    usage = if delivered_billable {
                        full_contract_usage()
                    } else {
                        TokenUsage::default()
                    };
                }
                Err(_) => {
                    terminal_status = 502;
                    error_code = Some("upstream_invalid_usage");
                    charge_contract_ceiling = delivered_billable;
                    usage = if delivered_billable {
                        full_contract_usage()
                    } else {
                        TokenUsage::default()
                    };
                }
            }
            let response_id = if (200..400).contains(&terminal_status)
                && matches!(protocol, Protocol::OpenAiResponses)
            {
                match sse_summary.as_ref().map(|summary| &summary.outcome) {
                    Some(ResponsesSseOutcome::Completed { response_id }) => response_id.clone(),
                    None => extract_response_id(&usage_capture),
                    Some(ResponsesSseOutcome::Failed | ResponsesSseOutcome::Incomplete) => None,
                }
            } else {
                None
            };
            let conversation_input =
                conversation
                    .as_ref()
                    .map(|conversation| ProxyConversationInput {
                        key: &conversation.key,
                        request_json: &conversation.request_json,
                        hints: &conversation.hints,
                        client_name: conversation.client_name.as_deref(),
                        upstream_response_id: response_id.as_deref(),
                    });
            let terminal_result = finish_proxy_request_with_archive_fallback(
                &background_state.db,
                FinishProxyRequest {
                    request_id,
                    tenant_id,
                    reservation: &reservation,
                    input_token_ceiling,
                    output_token_ceiling,
                    requested_service_tier: requested_service_tier.as_deref(),
                    status_code: terminal_status,
                    duration_ms: started.elapsed().as_millis() as i64,
                    usage,
                    charge_contract_ceiling,
                    error_code,
                    response_object: &stored_response,
                    conversation: conversation_input,
                },
                response_archive_attempt.as_ref(),
                &gap_response,
            )
            .await;
            if terminal_result.is_err() {
                // The commit can be durable even when its acknowledgement is lost.
                // Preserve this request-scoped archive until its database owner is
                // known; deleting it here could leave a committed row dangling.
                tracing::error!(%request_id, stage = "terminal_transaction", "proxy request finalization failed");
            }
        };
        if run_bounded_proxy_lifecycle(lifecycle_deadline, lifecycle)
            .await
            .is_err()
        {
            tracing::error!(
                %request_id,
                stage = "lifecycle_deadline",
                "proxy request lifecycle exceeded its absolute deadline"
            );
        }
    });
    let mut response = Response::builder()
        .status(status)
        .header(REQUEST_ID_HEADER, request_id.to_string());
    if let Some(content_type) = content_type {
        response = response.header(header::CONTENT_TYPE, content_type);
    }
    response
        .body(Body::from_stream(ReceiverStream::new(body_receiver)))
        .map_err(|_| AppError::Internal)
}
