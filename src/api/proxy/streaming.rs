use super::*;

mod archive;
mod terminal_delivery;

use archive::{cancel_stream_archive, stream_response_archive};
use terminal_delivery::{ResponsesTerminalDelivery, TerminalEof};

pub(super) struct StreamingResponse<'a> {
    pub(super) state: &'a AppState,
    pub(super) upstream: UpstreamResponse,
    pub(super) status: StatusCode,
    pub(super) content_type: Option<HeaderValue>,
    pub(super) is_sse: bool,
    pub(super) capture_json_usage: bool,
    pub(super) protocol: Protocol,
    pub(super) is_codex_route: bool,
    pub(super) codex_retry: CodexRetryTerminalGuard,
    pub(super) strict_openai_chat_usage: bool,
    pub(super) upstream_activity: crate::metrics::ActivityGuard,
    pub(super) request_id: Uuid,
    pub(super) buffered_request: BufferedRequest<'a>,
    pub(super) proxy_lifecycle_permit: tokio::sync::OwnedSemaphorePermit,
}

/// One upstream network chunk can contain several fully-framed SSE events.
/// Keep those immutable slices together so a capacity-one archive channel
/// cannot mistake intra-chunk framing for archive backpressure.
pub(super) struct ResponseArchiveBatch {
    chunks: Vec<Bytes>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ResponseArchiveBatchError {
    BatchLimit,
    Backpressure,
}

impl ResponseArchiveBatch {
    fn from_delivery_frames(
        frames: &[SseDeliveryFrame],
    ) -> Result<Option<Self>, ResponseArchiveBatchError> {
        if frames.is_empty() {
            return Ok(None);
        }
        let bytes = frames.iter().fold(0_usize, |total, frame| {
            total.saturating_add(frame.bytes.len())
        });
        if frames.len() > MAX_SSE_FRAMES_PER_NETWORK_CHUNK
            || bytes > MAX_PROXY_RESPONSE_BODY
            || (frames.len() > 1 && bytes > MAX_SSE_FRAMED_BYTES_PER_NETWORK_CHUNK)
        {
            return Err(ResponseArchiveBatchError::BatchLimit);
        }
        Ok(Some(Self {
            chunks: frames.iter().map(|frame| frame.bytes.clone()).collect(),
        }))
    }
}

fn try_queue_response_archive_batch(
    sender: &tokio::sync::mpsc::Sender<ResponseArchiveBatch>,
    frames: &[SseDeliveryFrame],
) -> Result<(), ResponseArchiveBatchError> {
    let Some(batch) = ResponseArchiveBatch::from_delivery_frames(frames)? else {
        return Ok(());
    };
    sender
        .try_send(batch)
        .map_err(|_| ResponseArchiveBatchError::Backpressure)
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
        mut codex_retry,
        strict_openai_chat_usage,
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
                    tokio::sync::mpsc::channel::<ResponseArchiveBatch>(PROXY_BODY_CHANNEL_CAPACITY);
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
            let mut sse_capture = is_sse.then(|| match protocol {
                Protocol::OpenAiChat if strict_openai_chat_usage => {
                    ResponsesSseCapture::for_openai_chat_usage()
                }
                Protocol::OpenAiResponses if is_codex_route => {
                    ResponsesSseCapture::for_codex_responses()
                }
                Protocol::OpenAiResponses => ResponsesSseCapture::for_responses(),
                _ => ResponsesSseCapture::for_delivery(),
            });
            let mut responses_streaming_sanitizer = (is_sse
                && matches!(protocol, Protocol::OpenAiResponses))
            .then(crate::api::sse::ResponsesStreamingSanitizer::default);
            let mut transport_error: Option<&'static str> = None;
            let mut response_bytes = 0_usize;
            let mut delivery_confirmed = false;
            let mut delivered_billable = false;
            let mut terminal_delivery = ResponsesTerminalDelivery::default();
            loop {
                let mut flushing_terminal = false;
                let next = if let Some(chunk) = terminal_delivery.take_pending() {
                    flushing_terminal = true;
                    Some(Ok(chunk))
                } else {
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
                    }
                };
                let Some(next) = next else {
                    match terminal_delivery.finish_at_eof(responses_streaming_sanitizer.as_mut()) {
                        TerminalEof::Flush => continue,
                        TerminalEof::Complete => break,
                        TerminalEof::Error(error_code) => {
                            transport_error = Some(error_code);
                            cancel_stream_archive(&archive_complete, &mut archive_sender);
                            let _ = tokio::time::timeout(
                                MAX_DOWNSTREAM_SEND_WAIT,
                                body_sender.send(Err(std::io::Error::other(
                                    "upstream Responses stream ended with an incomplete frame",
                                ))),
                            )
                            .await;
                        }
                    }
                    break;
                };
                match next {
                    Ok(raw_chunk) => {
                        let chunk = if flushing_terminal {
                            raw_chunk
                        } else {
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
                            if let Some(sanitizer) = responses_streaming_sanitizer.as_mut() {
                                match sanitizer.push(&raw_chunk) {
                                    Ok(chunk) => chunk,
                                    Err(error_code) => {
                                        transport_error = Some(error_code);
                                        cancel_stream_archive(
                                            &archive_complete,
                                            &mut archive_sender,
                                        );
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
                                raw_chunk
                            }
                        };
                        // A Responses sanitizer may need several network
                        // fragments before it can emit one complete, redacted
                        // SSE event. Empty partial output must not occupy the
                        // bounded archive channel or cancel a healthy archive.
                        if chunk.is_empty() {
                            continue;
                        }
                        if capture_json_usage {
                            append_bounded(&mut usage_capture, &chunk, 2 * 1024 * 1024);
                            capture_memory.set_bytes(usage_capture.capacity());
                        }
                        // The capture is the single, stateful SSE classifier for
                        // Responses and strict Chat. It emits whole events so a
                        // fragmented comment/control frame never confirms
                        // delivery or occupies the archive channel as output.
                        let delivery_frames = if let Some(capture) = sse_capture.as_mut() {
                            capture.push_delivery_frames(&chunk)
                        } else if chunk.is_empty() {
                            Vec::new()
                        } else {
                            vec![SseDeliveryFrame {
                                bytes: chunk,
                                billable: true,
                            }]
                        };
                        let chat_usage_done = sse_capture
                            .as_ref()
                            .is_some_and(ResponsesSseCapture::chat_usage_done);
                        // Strict Chat emits its terminal usage before `[DONE]`,
                        // so its protocol contract is complete at the sentinel.
                        // Responses must still consume EOF: the sanitizer rejects
                        // a trailing partial frame after a valid terminal event.
                        let strict_chat_done = strict_openai_chat_usage
                            && sse_capture.as_ref().is_some_and(|capture| {
                                capture.saw_done() && !capture.has_pending_crlf_continuation()
                            });
                        if let Some(sender) = archive_sender.as_ref()
                            && try_queue_response_archive_batch(sender, &delivery_frames).is_err()
                        {
                            tracing::warn!(%request_id, stage = "response_archive_backpressure", "proxy archive gap");
                            cancel_stream_archive(&archive_complete, &mut archive_sender);
                        }
                        for frame in delivery_frames {
                            let SseDeliveryFrame { bytes, billable } = frame;
                            if billable && !delivery_confirmed {
                                // `delivery_started` is the durable signal the orphan reaper
                                // uses to charge a stranded stream. Confirm it before any
                                // billable byte is enqueued, while control frames never reach
                                // this transition.
                                match tokio::time::timeout(
                                    MAX_DOWNSTREAM_SEND_WAIT,
                                    body_sender.reserve(),
                                )
                                .await
                                {
                                    Ok(Ok(permit)) => match prepare_proxy_delivery_with_retry(
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
                                            if confirm_proxy_delivery_with_retry(
                                                &background_state.db,
                                                request_id,
                                                tenant_id,
                                                &reservation,
                                            )
                                            .await
                                            .is_err()
                                            {
                                                drop(permit);
                                                transport_error = Some("delivery_state");
                                            } else {
                                                delivery_confirmed = true;
                                                delivered_billable = true;
                                                permit.send(Ok::<Bytes, std::io::Error>(bytes));
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
                                    },
                                    Ok(Err(_)) => transport_error = Some("downstream_disconnected"),
                                    Err(_) => transport_error = Some("downstream_backpressure"),
                                }
                            } else {
                                match tokio::time::timeout(
                                    MAX_DOWNSTREAM_SEND_WAIT,
                                    body_sender.send(Ok::<Bytes, std::io::Error>(bytes)),
                                )
                                .await
                                {
                                    Ok(Ok(())) => delivered_billable |= billable,
                                    Ok(Err(_)) => transport_error = Some("downstream_disconnected"),
                                    Err(_) => transport_error = Some("downstream_backpressure"),
                                }
                            }
                            if transport_error.is_some() {
                                break;
                            }
                        }
                        if transport_error.is_some() {
                            cancel_stream_archive(&archive_complete, &mut archive_sender);
                            break;
                        }
                        if chat_usage_done || strict_chat_done {
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
            let sse_summary = sse_capture.map(ResponsesSseCapture::finish_summary);
            if matches!(
                sse_summary.as_ref().map(|summary| &summary.outcome),
                Some(ResponsesSseOutcome::Incomplete)
            ) {
                // A partial SSE event is not a deliverable response and must
                // not leave a complete-looking archive prefix behind.
                cancel_stream_archive(&archive_complete, &mut archive_sender);
            }
            // EOF is part of downstream delivery. Close it before awaiting the
            // archive sidecar or terminal settlement so neither can prolong
            // the client-visible stream lifetime.
            drop(body_sender);
            drop(archive_sender.take());
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
            // A retry's success is a protocol-terminal property, not a 2xx
            // header or SSE framing property. Direct Codex streams must have
            // exactly one matching `response.completed` carrying a stable
            // response id; capture marks duplicate/mismatched terminal events
            // incomplete before this point.
            let retry_terminal = if is_codex_route
                && error_code.is_none()
                && (200..400).contains(&terminal_status)
                && matches!(
                    sse_summary.as_ref().map(|summary| &summary.outcome),
                    Some(ResponsesSseOutcome::Completed {
                        response_id: Some(_)
                    })
                ) {
                CodexRetryTerminal::Succeeded
            } else if matches!(
                transport_error,
                Some("downstream_disconnected" | "downstream_backpressure")
            ) {
                CodexRetryTerminal::Cancelled
            } else {
                CodexRetryTerminal::Failed
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
            let terminal_result_failed = terminal_result.is_err();
            if terminal_result_failed {
                // The commit can be durable even when its acknowledgement is lost.
                // Preserve this request-scoped archive until its database owner is
                // known; deleting it here could leave a committed row dangling.
                tracing::error!(%request_id, stage = "terminal_transaction", "proxy request finalization failed");
            }
            codex_retry.complete(if terminal_result_failed {
                CodexRetryTerminal::Failed
            } else {
                retry_terminal
            });
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

#[cfg(test)]
#[path = "streaming/tests.rs"]
mod tests;
