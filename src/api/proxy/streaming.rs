use super::*;

mod delivery;
mod lifecycle;
mod terminal_delivery;
#[cfg(test)]
mod tests;

use delivery::{CapturedSseDelivery, capture_sse_delivery, downstream_stream_failure};
use lifecycle::{StreamingFinalizationInput, finalize_streaming_lifecycle};
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
    pub(super) upstream_attempt: UpstreamAttemptGuard,
    pub(super) strict_openai_chat_usage: bool,
    pub(super) upstream_activity: crate::metrics::ActivityGuard,
    pub(super) request_id: Uuid,
    /// Stable operator-only correlation metadata. This is intentionally an
    /// account UUID rather than any provider response field so protocol
    /// rejections can be diagnosed without retaining or logging upstream
    /// content.
    pub(super) upstream_account_id: Uuid,
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
        codex_retry,
        mut upstream_attempt,
        strict_openai_chat_usage,
        upstream_activity,
        request_id,
        upstream_account_id,
        buffered_request,
        proxy_lifecycle_permit,
    } = input;
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
        // The bounded lifecycle below owns these values. Keep exact copies for
        // the timeout convergence path, which must not infer delivery from a
        // task that Tokio has just cancelled.
        let deadline_database = background_state.db.clone();
        let deadline_metrics = background_state.metrics.clone();
        let deadline_reservation = reservation.clone();
        let deadline_tenant_id = tenant_id;
        let deadline_started = started;
        let lifecycle = async move {
            let mut upstream_stream = upstream.bytes_stream();
            let spool_identity = crate::db::ArchiveSpoolIdentity {
                request_id,
                tenant_id,
                reservation_id: reservation.id,
            };
            let mut archive_sender = crate::response_archive_spool::ResponseArchiveProducer::begin(
                &background_state,
                spool_identity,
            )
            .await;
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
            let mut terminal_frames = delivery::TerminalFrames::default();
            let mut terminal_memory = background_state
                .metrics
                .memory_usage(crate::metrics::MemoryComponent::StreamCapture, 0);
            loop {
                let mut flushing_terminal = false;
                let next = if let Some(chunk) = terminal_delivery.take_pending() {
                    flushing_terminal = true;
                    Some(Ok(chunk))
                } else if !terminal_delivery.upstream_poll_allowed() {
                    break;
                } else {
                    match tokio::time::timeout_at(stream_deadline, upstream_stream.next()).await {
                        Ok(next) => next,
                        Err(_) => {
                            transport_error = Some("upstream_timeout");
                            drop(archive_sender.take());
                            let _ = tokio::time::timeout(
                                MAX_DOWNSTREAM_SEND_WAIT,
                                body_sender.send(downstream_stream_failure(
                                    protocol,
                                    is_sse,
                                    responses_streaming_sanitizer.as_ref(),
                                    "upstream stream timed out",
                                )),
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
                            let protocol_rejection_stage = responses_streaming_sanitizer
                                .as_ref()
                                .map_or("unknown", |sanitizer| sanitizer.last_rejection_stage());
                            tracing::warn!(
                                %request_id,
                                %upstream_account_id,
                                stage = error_code,
                                protocol_rejection_stage,
                                "Responses upstream stream rejected at EOF"
                            );
                            transport_error = Some(error_code);
                            drop(archive_sender.take());
                            let _ = tokio::time::timeout(
                                MAX_DOWNSTREAM_SEND_WAIT,
                                body_sender.send(downstream_stream_failure(
                                    protocol,
                                    is_sse,
                                    responses_streaming_sanitizer.as_ref(),
                                    "upstream Responses stream ended with an incomplete frame",
                                )),
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
                                drop(archive_sender.take());
                                let _ = tokio::time::timeout(
                                    MAX_DOWNSTREAM_SEND_WAIT,
                                    body_sender.send(downstream_stream_failure(
                                        protocol,
                                        is_sse,
                                        responses_streaming_sanitizer.as_ref(),
                                        "upstream response exceeded the size limit",
                                    )),
                                )
                                .await;
                                break;
                            }
                            if let Some(sanitizer) = responses_streaming_sanitizer.as_mut() {
                                match sanitizer.push(&raw_chunk) {
                                    Ok(chunk) => chunk,
                                    Err(error_code) => {
                                        // `error_code` is a fixed parser
                                        // classification, never an upstream
                                        // string or payload. Keep this at the
                                        // rejection boundary: once the safe
                                        // terminal frame is emitted, archive
                                        // backpressure may legitimately leave
                                        // no raw response to inspect.
                                        tracing::warn!(
                                            %request_id,
                                            %upstream_account_id,
                                            stage = error_code,
                                            protocol_rejection_stage = sanitizer.last_rejection_stage(),
                                            "Responses upstream stream rejected by protocol sanitizer"
                                        );
                                        transport_error = Some(error_code);
                                        drop(archive_sender.take());
                                        let _ = tokio::time::timeout(
                                            MAX_DOWNSTREAM_SEND_WAIT,
                                            body_sender.send(downstream_stream_failure(
                                                protocol,
                                                is_sse,
                                                Some(&*sanitizer),
                                                "upstream stream violated the Responses protocol",
                                            )),
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
                        let CapturedSseDelivery {
                            frames: delivery_frames,
                            strict_chat_terminal_ready,
                        } = match capture_sse_delivery(
                            sse_capture.as_mut(),
                            chunk,
                            strict_openai_chat_usage,
                        ) {
                            Ok(delivery) => delivery,
                            Err(rejection) => {
                                transport_error = Some(rejection.error_code());
                                drop(archive_sender.take());
                                let _ = tokio::time::timeout(
                                    MAX_DOWNSTREAM_SEND_WAIT,
                                    body_sender.send(downstream_stream_failure(
                                        protocol,
                                        is_sse,
                                        responses_streaming_sanitizer.as_ref(),
                                        "upstream SSE stream exceeded framing limits",
                                    )),
                                )
                                .await;
                                break;
                            }
                        };
                        if let Some(spool) = archive_sender.as_mut()
                            && !spool
                                .append(delivery_frames.iter().map(|f| f.bytes.clone()).collect())
                                .await
                        {
                            tracing::warn!(%request_id, stage = "response_spool_ack", "proxy archive gap");
                            drop(archive_sender.take());
                        }
                        for frame in delivery_frames {
                            let frame = match terminal_frames.hold(frame) {
                                Ok(Some(frame)) => frame,
                                Ok(None) => {
                                    terminal_memory.set_bytes(terminal_frames.bytes());
                                    continue;
                                }
                                Err(()) => {
                                    transport_error =
                                        Some("upstream_response_event_batch_too_large");
                                    break;
                                }
                            };
                            match delivery::send_frame(
                                delivery::FrameDelivery {
                                    state: &background_state,
                                    sender: &body_sender,
                                    request_id,
                                    tenant_id,
                                    reservation: &reservation,
                                    input_token_ceiling,
                                    output_token_ceiling,
                                    requested_service_tier: requested_service_tier.as_deref(),
                                    confirmed: &mut delivery_confirmed,
                                    probe: ((matches!(protocol, Protocol::OpenAiResponses)
                                        || strict_openai_chat_usage)
                                        && sse_capture.as_ref().is_some_and(
                                            ResponsesSseCapture::can_confirm_probe_delivery,
                                        ))
                                    .then_some(&mut upstream_attempt),
                                },
                                frame,
                            )
                            .await
                            {
                                Ok(billable) => delivered_billable |= billable,
                                Err(error) => {
                                    transport_error = Some(error);
                                    break;
                                }
                            }
                        }
                        if transport_error.is_some() {
                            drop(archive_sender.take());
                            if transport_error == Some("delivery_state") {
                                let _ = tokio::time::timeout(
                                    MAX_DOWNSTREAM_SEND_WAIT,
                                    body_sender.send(downstream_stream_failure(
                                        protocol,
                                        is_sse,
                                        responses_streaming_sanitizer.as_ref(),
                                        "response delivery could not be recorded",
                                    )),
                                )
                                .await;
                            }
                            break;
                        }
                        if strict_chat_terminal_ready {
                            break;
                        }
                    }
                    Err(_) => {
                        transport_error = Some("upstream_stream");
                        drop(archive_sender.take());
                        let _ = tokio::time::timeout(
                            MAX_DOWNSTREAM_SEND_WAIT,
                            body_sender.send(downstream_stream_failure(
                                protocol,
                                is_sse,
                                responses_streaming_sanitizer.as_ref(),
                                "upstream stream failed",
                            )),
                        )
                        .await;
                        break;
                    }
                }
            }
            if transport_error.is_some() {
                drop(archive_sender.take());
            }
            let sse_summary = sse_capture.map(ResponsesSseCapture::finish_summary);
            let incomplete = matches!(
                sse_summary.as_ref().map(|summary| &summary.outcome),
                Some(ResponsesSseOutcome::Incomplete)
            );
            if incomplete {
                // A partial SSE event is not a deliverable response and must
                // not leave a complete-looking archive prefix behind.
                drop(archive_sender.take());
            }
            // Success terminals and EOF are downstream commit markers. First
            // make the complete capture recoverable (or record an honest gap).
            // S3 upload remains asynchronous and is never awaited here.
            let spool_sealed = match archive_sender.take() {
                Some(spool) => spool.seal().await,
                None => false,
            };
            if !spool_sealed {
                crate::response_archive_spool::mark_gap(
                    &background_state,
                    spool_identity,
                    "capture_failed",
                )
                .await;
            }
            let failed = matches!(
                sse_summary.as_ref().map(|summary| &summary.outcome),
                Some(ResponsesSseOutcome::Failed)
            );
            if transport_error.is_none() && (incomplete || (failed && !terminal_frames.is_empty()))
            {
                // A later malformed tail or failure invalidates a held success.
                // Do not ask the sanitizer whether an error was already emitted:
                // that error may itself be held behind the revoked success.
                let _ = tokio::time::timeout(
                    MAX_DOWNSTREAM_SEND_WAIT,
                    body_sender.send(Ok(delivery::invalid_terminal_failure(protocol))),
                )
                .await;
            } else if transport_error.is_none() {
                for frame in terminal_frames.take() {
                    match delivery::send_frame(
                        delivery::FrameDelivery {
                            state: &background_state,
                            sender: &body_sender,
                            request_id,
                            tenant_id,
                            reservation: &reservation,
                            input_token_ceiling,
                            output_token_ceiling,
                            requested_service_tier: requested_service_tier.as_deref(),
                            confirmed: &mut delivery_confirmed,
                            probe: (matches!(protocol, Protocol::OpenAiResponses)
                                || strict_openai_chat_usage)
                                .then_some(&mut upstream_attempt),
                        },
                        frame,
                    )
                    .await
                    {
                        Ok(billable) => delivered_billable |= billable,
                        Err(error) => {
                            transport_error = Some(error);
                            break;
                        }
                    }
                }
            }
            drop(body_sender);
            drop(terminal_frames);
            terminal_memory.set_bytes(0);
            let gap_response = format!("gap://{request_id}/response");
            let stored_response = gap_response.clone();
            let response_archive_attempt = None;
            finalize_streaming_lifecycle(StreamingFinalizationInput {
                state: &background_state,
                status_code,
                protocol,
                is_codex_route,
                codex_retry,
                upstream_attempt,
                request_id,
                reservation,
                started,
                input_token_ceiling,
                output_token_ceiling,
                requested_service_tier,
                conversation,
                tenant_id,
                transport_error,
                delivered_billable,
                sse_summary,
                usage_capture,
                response_archive_attempt,
                stored_response,
                gap_response,
            })
            .await;
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
            match deadline_database
                .expire_proxy_lifecycle_deadline(
                    request_id,
                    deadline_tenant_id,
                    &deadline_reservation,
                    deadline_started.elapsed().as_millis() as i64,
                )
                .await
            {
                Ok(FinishProxyRequestResult::Finished { .. }) => {
                    deadline_metrics.observe_proxy_lifecycle_deadline(
                        crate::metrics::ProxyLifecycleDeadlineOutcome::Converged,
                    );
                    tracing::warn!(
                        %request_id,
                        error_code = "request_lifecycle_timeout",
                        "proxy lifecycle deadline converged a pending request"
                    );
                }
                Ok(FinishProxyRequestResult::AlreadyFinished { .. }) => {}
                Err(error) => {
                    deadline_metrics.observe_proxy_lifecycle_deadline(
                        crate::metrics::ProxyLifecycleDeadlineOutcome::ReconcileFailed,
                    );
                    tracing::error!(
                        %request_id,
                        error_code = "request_lifecycle_timeout_reconcile_failed",
                        %error,
                        "proxy lifecycle deadline could not converge a pending request"
                    );
                }
            }
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
