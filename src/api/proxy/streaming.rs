use super::*;

mod delivery;
mod lifecycle;
mod terminal_delivery;
#[cfg(test)]
mod tests;
mod timing;

use delivery::{CapturedSseDelivery, capture_sse_delivery, downstream_stream_failure};
use lifecycle::{StreamingFinalizationInput, finalize_streaming_lifecycle};
use terminal_delivery::{ResponsesTerminalDelivery, TerminalEof};

enum DownstreamAwarePoll<T> {
    Upstream { value: T, downstream_closed: bool },
    DownstreamClosed,
}

enum StreamPoll<T> {
    Upstream(DownstreamAwarePoll<T>),
    ProgressHeartbeat,
    TimedOut,
}

// Codex treats an SSE connection with no Responses protocol event for about
// five minutes as stalled. Keep a generous margin below that client limit
// while avoiding a material per-request event rate.
const CODEX_RESPONSES_PROGRESS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

async fn poll_upstream_or_downstream_closed<T>(
    body_sender: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    upstream: impl std::future::Future<Output = T>,
) -> DownstreamAwarePoll<T> {
    tokio::select! {
        // If both sides became ready before this poll, retain the already
        // available upstream item. It may contain authoritative completed
        // usage needed for settlement. The caller observes `is_closed` and
        // only polls again to finish protocol evidence already buffered by the
        // sanitizer; a pending provider read loses immediately to `closed`.
        biased;
        value = upstream => DownstreamAwarePoll::Upstream {
            value,
            downstream_closed: body_sender.is_closed(),
        },
        _ = body_sender.closed() => DownstreamAwarePoll::DownstreamClosed,
    }
}

async fn poll_upstream_downstream_or_progress_heartbeat<T>(
    body_sender: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    upstream: impl std::future::Future<Output = T>,
    stream_deadline: tokio::time::Instant,
    progress_heartbeat_deadline: Option<tokio::time::Instant>,
) -> StreamPoll<T> {
    let upstream_poll = tokio::time::timeout_at(
        stream_deadline,
        poll_upstream_or_downstream_closed(body_sender, upstream),
    );
    tokio::pin!(upstream_poll);
    if let Some(progress_heartbeat_deadline) = progress_heartbeat_deadline {
        tokio::select! {
            biased;
            result = &mut upstream_poll => match result {
                Ok(poll) => StreamPoll::Upstream(poll),
                Err(_) => StreamPoll::TimedOut,
            },
            _ = tokio::time::sleep_until(progress_heartbeat_deadline) => StreamPoll::ProgressHeartbeat,
        }
    } else {
        match upstream_poll.await {
            Ok(poll) => StreamPoll::Upstream(poll),
            Err(_) => StreamPoll::TimedOut,
        }
    }
}

fn transport_error_with_downstream_precedence(
    downstream_closed: bool,
    error: &'static str,
) -> &'static str {
    if downstream_closed {
        "downstream_disconnected"
    } else {
        error
    }
}

fn pending_delivery_can_advance(
    sanitizer: Option<&crate::api::sse::ResponsesStreamingSanitizer>,
    raw_chunk_len: usize,
) -> bool {
    raw_chunk_len > 0 && sanitizer.is_some_and(|sanitizer| sanitizer.has_pending_delivery())
}

fn resume_pending_delivery_after_send_disconnect(
    transport_error: &mut Option<&'static str>,
    downstream_closed: &mut bool,
    sanitizer: Option<&crate::api::sse::ResponsesStreamingSanitizer>,
) -> bool {
    if *transport_error != Some("downstream_disconnected")
        || !sanitizer.is_some_and(|sanitizer| sanitizer.has_pending_delivery())
    {
        return false;
    }
    *downstream_closed = true;
    *transport_error = None;
    true
}

pub(super) struct StreamingResponse<'a> {
    pub(super) state: &'a AppState,
    pub(super) upstream: UpstreamResponse,
    pub(super) status: StatusCode,
    pub(super) content_type: Option<HeaderValue>,
    pub(super) is_sse: bool,
    pub(super) capture_json_usage: bool,
    pub(super) protocol: Protocol,
    pub(super) is_codex_route: bool,
    pub(super) is_kimi_route: bool,
    pub(super) codex_retry: CodexRetryTerminalGuard,
    pub(super) codex_chat_model: Option<String>,
    pub(super) codex_chat_include_usage: bool,
    pub(super) upstream_attempt: UpstreamAttemptGuard,
    pub(super) strict_openai_chat_usage: bool,
    pub(super) upstream_activity: crate::metrics::ActivityGuard,
    pub(super) request_id: Uuid,
    /// Stable operator-only correlation metadata. This is intentionally an
    /// account UUID rather than any provider response field so protocol
    /// rejections can be diagnosed without retaining or logging upstream
    /// content.
    pub(super) upstream_account_id: Uuid,
    pub(super) credential_generation: i64,
    pub(super) buffered_request: BufferedRequest<'a>,
    pub(super) proxy_lifecycle_permit: tokio::sync::OwnedSemaphorePermit,
}

pub(super) async fn stream_response(input: StreamingResponse<'_>) -> Result<Response, AppError> {
    let diagnostic_context = proxy_diagnostics::Context::for_request(input.request_id);
    let StreamingResponse {
        state,
        upstream,
        status,
        content_type,
        is_sse,
        capture_json_usage,
        protocol,
        is_codex_route,
        is_kimi_route,
        codex_retry,
        codex_chat_model,
        codex_chat_include_usage,
        mut upstream_attempt,
        strict_openai_chat_usage,
        upstream_activity,
        request_id,
        upstream_account_id,
        credential_generation,
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
        memory,
        ..
    } = buffered_request;
    tokio::spawn(async move {
        let stream_owner = proxy_diagnostics::Phase::account(
            diagnostic_context,
            "stream_owner",
            Some(upstream_account_id),
            Some(credential_generation),
        );
        // Streaming responses outlive the handler response. Keep the workload
        // permit until proxy finalization or timeout reconciliation; accepted
        // archive tails have a separate bounded EOF owner below.
        let _proxy_lifecycle_permit = proxy_lifecycle_permit;
        let archive_memory = memory.clone();
        let request_memory = memory;
        let _stream_activity = stream_activity;
        let _upstream_activity = upstream_activity;
        let lifecycle_started = tokio::time::Instant::now();
        let stream_deadline = lifecycle_started + MAX_PROXY_STREAM_LIFETIME;
        let lifecycle_deadline = lifecycle_started + MAX_PROXY_LIFETIME;
        let (archive_settlement_sender, archive_settlement_receiver) =
            tokio::sync::oneshot::channel();
        let archive_eof_owner = tokio::spawn(hold_response_eof_until_archive_settles(
            archive_settlement_receiver,
            body_sender.clone(),
            diagnostic_context,
        ));
        // The bounded lifecycle below owns these values. Keep exact copies for
        // the timeout convergence path, which must not infer delivery from a
        // task that Tokio has just cancelled.
        let deadline_database = background_state.db.clone();
        let deadline_metrics = background_state.metrics.clone();
        let deadline_reservation = reservation.clone();
        let deadline_tenant_id = tenant_id;
        let deadline_started = started;
        let lifecycle = async move {
            let stream_phase = proxy_diagnostics::Phase::account(
                diagnostic_context,
                "upstream_stream",
                Some(upstream_account_id),
                Some(credential_generation),
            );
            let mut first_byte = Some(proxy_diagnostics::Phase::account(
                diagnostic_context,
                "stream_first_byte",
                Some(upstream_account_id),
                Some(credential_generation),
            ));
            let mut upstream_stream = upstream.bytes_stream();
            let spool_identity = crate::db::ArchiveSpoolIdentity {
                request_id,
                tenant_id,
                reservation_id: reservation.id,
            };
            let mut archive_sender = crate::response_archive_spool::ResponseArchiveProducer::begin(
                &background_state,
                spool_identity,
                archive_memory,
            );
            let mut usage_capture = Vec::new();
            let mut capture_memory = background_state
                .metrics
                .memory_usage(crate::metrics::MemoryComponent::StreamCapture, 0);
            let mut codex_chat_translator = codex_chat_model.map(|model| {
                codex_transport::CodexChatStreamTranslator::new(
                    request_id,
                    model,
                    codex_chat_include_usage,
                )
            });
            let mut sse_capture = is_sse.then(|| match protocol {
                Protocol::OpenAiChat if is_codex_route => {
                    ResponsesSseCapture::for_codex_responses()
                }
                Protocol::OpenAiChat if strict_openai_chat_usage => {
                    chat_usage_capture(is_kimi_route)
                }
                Protocol::OpenAiResponses if is_codex_route => {
                    ResponsesSseCapture::for_codex_responses()
                }
                Protocol::OpenAiResponses => ResponsesSseCapture::for_responses(),
                _ => ResponsesSseCapture::for_delivery(),
            });
            let mut responses_streaming_sanitizer = (is_sse
                && (is_codex_route || matches!(protocol, Protocol::OpenAiResponses)))
            .then(crate::api::sse::ResponsesStreamingSanitizer::default);
            let codex_responses_progress_heartbeat =
                is_sse && is_codex_route && matches!(protocol, Protocol::OpenAiResponses);
            let mut transport_error: Option<&'static str> = None;
            let mut response_bytes = 0_usize;
            let mut delivery_confirmed = false;
            let mut delivered_billable = false;
            let mut terminal_delivery = ResponsesTerminalDelivery::default();
            let mut terminal_frames = delivery::TerminalFrames::default();
            let mut output_timing = timing::OutputTiming::default();
            let mut terminal_memory = background_state
                .metrics
                .memory_usage(crate::metrics::MemoryComponent::StreamCapture, 0);
            let mut downstream_closed_observed = false;
            let mut downstream_ready_bytes = 0_usize;
            // This stays `None` until a validated lifecycle identity is safe
            // to expose downstream. A successful terminal may provide that
            // first identity while it remains privately held for EOF. The
            // synthetic event is downstream-only, so it cannot alter archive
            // contents, usage, settlement, or the delivery transition.
            let mut progress_heartbeat_deadline = None;
            loop {
                let mut flushing_terminal = false;
                let next = if let Some(chunk) = terminal_delivery.take_pending() {
                    flushing_terminal = true;
                    Some(Ok(chunk))
                } else if !terminal_delivery.upstream_poll_allowed() {
                    break;
                } else {
                    match poll_upstream_downstream_or_progress_heartbeat(
                        &body_sender,
                        upstream_stream.next(),
                        stream_deadline,
                        progress_heartbeat_deadline,
                    )
                    .await
                    {
                        StreamPoll::Upstream(DownstreamAwarePoll::Upstream {
                            value: next,
                            downstream_closed,
                        }) => {
                            downstream_closed_observed |= downstream_closed;
                            if downstream_closed {
                                drop(archive_sender.take());
                            }
                            next
                        }
                        StreamPoll::Upstream(DownstreamAwarePoll::DownstreamClosed) => {
                            transport_error = Some("downstream_disconnected");
                            drop(archive_sender.take());
                            break;
                        }
                        StreamPoll::ProgressHeartbeat => {
                            let Some(heartbeat) = responses_streaming_sanitizer.as_ref().and_then(
                                crate::api::sse::ResponsesStreamingSanitizer::progress_heartbeat,
                            ) else {
                                progress_heartbeat_deadline = None;
                                continue;
                            };
                            match tokio::time::timeout(
                                MAX_DOWNSTREAM_SEND_WAIT,
                                body_sender.send(Ok(heartbeat)),
                            )
                            .await
                            {
                                Ok(Ok(())) => {
                                    progress_heartbeat_deadline = Some(
                                        tokio::time::Instant::now()
                                            + CODEX_RESPONSES_PROGRESS_HEARTBEAT_INTERVAL,
                                    );
                                    continue;
                                }
                                Ok(Err(_)) => transport_error = Some("downstream_disconnected"),
                                Err(_) => transport_error = Some("downstream_backpressure"),
                            }
                            drop(archive_sender.take());
                            break;
                        }
                        StreamPoll::TimedOut => {
                            transport_error = Some(transport_error_with_downstream_precedence(
                                downstream_closed_observed || body_sender.is_closed(),
                                "upstream_timeout",
                            ));
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
                        TerminalEof::Complete => {
                            if downstream_closed_observed {
                                transport_error = Some("downstream_disconnected");
                            }
                            break;
                        }
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
                            transport_error = Some(transport_error_with_downstream_precedence(
                                downstream_closed_observed || body_sender.is_closed(),
                                error_code,
                            ));
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
                        let raw_chunk_len = raw_chunk.len();
                        if downstream_closed_observed && !flushing_terminal {
                            downstream_ready_bytes =
                                downstream_ready_bytes.saturating_add(raw_chunk_len);
                            if downstream_ready_bytes
                                > crate::api::limits::MAX_RESPONSES_SSE_TERMINAL_HOLD_BYTES
                            {
                                transport_error = Some("downstream_disconnected");
                                drop(archive_sender.take());
                                break;
                            }
                        }
                        if !raw_chunk.is_empty()
                            && let Some(phase) = first_byte.take()
                        {
                            phase.finish("received", Some(status.as_u16()), Some(raw_chunk.len()));
                        }
                        let chunk = if flushing_terminal {
                            raw_chunk
                        } else {
                            let _response_buffer = background_state.metrics.memory_usage(
                                crate::metrics::MemoryComponent::ResponseBuffer,
                                raw_chunk.len(),
                            );
                            response_bytes = response_bytes.saturating_add(raw_chunk.len());
                            if response_bytes > MAX_PROXY_RESPONSE_BODY {
                                transport_error = Some(transport_error_with_downstream_precedence(
                                    downstream_closed_observed || body_sender.is_closed(),
                                    "upstream_response_too_large",
                                ));
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
                                        transport_error =
                                            Some(transport_error_with_downstream_precedence(
                                                downstream_closed_observed
                                                    || body_sender.is_closed(),
                                                error_code,
                                            ));
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
                            // A valid success terminal is held until EOF and
                            // can therefore be the first event to establish a
                            // real response identity. Arm its downstream-only
                            // heartbeat even though no provider bytes are ready
                            // to deliver yet.
                            if codex_responses_progress_heartbeat
                                && progress_heartbeat_deadline.is_none()
                                && let Some(sanitizer) = responses_streaming_sanitizer.as_ref()
                                && sanitizer.progress_heartbeat().is_some()
                            {
                                progress_heartbeat_deadline = Some(
                                    tokio::time::Instant::now()
                                        + CODEX_RESPONSES_PROGRESS_HEARTBEAT_INTERVAL,
                                );
                            }
                            let terminal_evidence_pending = pending_delivery_can_advance(
                                responses_streaming_sanitizer.as_ref(),
                                raw_chunk_len,
                            );
                            if downstream_closed_observed && !terminal_evidence_pending {
                                transport_error = Some("downstream_disconnected");
                                drop(archive_sender.take());
                                break;
                            }
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
                            frames: mut delivery_frames,
                            strict_chat_terminal_ready,
                        } = match capture_sse_delivery(
                            sse_capture.as_mut(),
                            chunk,
                            strict_openai_chat_usage,
                        ) {
                            Ok(delivery) => delivery,
                            Err(rejection) => {
                                transport_error = Some(transport_error_with_downstream_precedence(
                                    downstream_closed_observed || body_sender.is_closed(),
                                    rejection.error_code(),
                                ));
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
                        if let Some(translator) = codex_chat_translator.as_mut() {
                            let mut translated = Vec::with_capacity(delivery_frames.len());
                            for mut frame in delivery_frames {
                                match translator.translate_frame(&frame.bytes) {
                                    Ok(Some(bytes)) => {
                                        frame.bytes = bytes;
                                        translated.push(frame);
                                    }
                                    Ok(None) => {}
                                    Err(error_code) => {
                                        transport_error =
                                            Some(transport_error_with_downstream_precedence(
                                                downstream_closed_observed
                                                    || body_sender.is_closed(),
                                                error_code,
                                            ));
                                        drop(archive_sender.take());
                                        let _ = tokio::time::timeout(
                                            MAX_DOWNSTREAM_SEND_WAIT,
                                            body_sender.send(downstream_stream_failure(
                                                protocol,
                                                is_sse,
                                                responses_streaming_sanitizer.as_ref(),
                                                "upstream response could not be represented as Chat Completions",
                                            )),
                                        )
                                        .await;
                                        break;
                                    }
                                }
                            }
                            if transport_error.is_some() {
                                break;
                            }
                            delivery_frames = translated;
                        }
                        let observed_ms = diagnostic_context.elapsed_millis_at(Instant::now());
                        for frame in &delivery_frames {
                            output_timing.observe(&frame.bytes, frame.terminal, observed_ms);
                        }
                        if let Some(spool) = archive_sender.as_mut()
                            && !spool
                                .append(delivery_frames.iter().map(|f| f.bytes.clone()).collect())
                        {
                            tracing::warn!(%request_id, stage = "response_spool_ack", "proxy archive gap");
                            drop(archive_sender.take());
                        }
                        // A successful Responses terminal is held privately
                        // until EOF validates its tail. Once the receiver is
                        // gone, do not attempt more downstream sends, but do
                        // allow immediately-ready fragments/EOF to complete
                        // that already-started evidence. The next pending read
                        // is interrupted by `body_sender.closed()` above.
                        if downstream_closed_observed
                            && responses_streaming_sanitizer
                                .as_ref()
                                .is_some_and(|sanitizer| sanitizer.has_pending_delivery())
                        {
                            continue;
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
                                        Some(transport_error_with_downstream_precedence(
                                            downstream_closed_observed || body_sender.is_closed(),
                                            "upstream_response_event_batch_too_large",
                                        ));
                                    break;
                                }
                            };
                            match delivery::send_frame(
                                delivery::FrameDelivery {
                                    diagnostic_context,
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
                                        || strict_openai_chat_usage
                                        || is_codex_route)
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
                            if resume_pending_delivery_after_send_disconnect(
                                &mut transport_error,
                                &mut downstream_closed_observed,
                                responses_streaming_sanitizer.as_ref(),
                            ) {
                                // The receiver can disappear after the poll
                                // snapshot, while an earlier part of this same
                                // raw chunk is being delivered. Preserve a
                                // success terminal already held from the rest
                                // of the chunk under the same immediate-ready
                                // and byte-bounded rule used above.
                                continue;
                            }
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
                        if downstream_closed_observed {
                            transport_error = Some("downstream_disconnected");
                            drop(archive_sender.take());
                            break;
                        }
                        if codex_responses_progress_heartbeat
                            && progress_heartbeat_deadline.is_none()
                            && let Some(sanitizer) = responses_streaming_sanitizer.as_ref()
                            && sanitizer.progress_heartbeat().is_some()
                        {
                            progress_heartbeat_deadline = Some(
                                tokio::time::Instant::now()
                                    + CODEX_RESPONSES_PROGRESS_HEARTBEAT_INTERVAL,
                            );
                        }
                        if strict_chat_terminal_ready {
                            break;
                        }
                    }
                    Err(error_code) => {
                        transport_error = Some(transport_error_with_downstream_precedence(
                            downstream_closed_observed || body_sender.is_closed(),
                            error_code,
                        ));
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
            stream_phase.finish(
                transport_error.unwrap_or("completed"),
                Some(status.as_u16()),
                Some(response_bytes),
            );
            if let Some(phase) = first_byte.take() {
                phase.finish("no_bytes", Some(status.as_u16()), Some(0));
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
            // Success terminals and EOF transfer the bounded capture to its
            // owned writer. The terminal frame does not wait for database
            // drain; the writer keeps the row capturing until seal commits.
            let terminal_delivery_phase =
                proxy_diagnostics::Phase::new(diagnostic_context, "terminal_delivery");
            let archive_settlement: Option<
                crate::response_archive_spool::ResponseArchiveSettlement,
            > = match archive_sender.take() {
                Some(spool) => spool.seal(),
                None => None,
            };
            if let Err(unowned) = archive_settlement_sender.send(archive_settlement)
                && let Some(settlement) = unowned
            {
                // The EOF owner contains no fallible work before receiving, so
                // this is defensive. Retain ownership locally if it exited.
                settlement.wait().await;
            }
            // A spawned writer exclusively fences its failed/abandoned
            // capture, including a begin that commits after cancellation.
            // No writer means memory admission failed before any spool SQL;
            // the request already has its gap locator. Do not duplicate the
            // fence or put its database wait ahead of terminal delivery.
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
                            diagnostic_context,
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
                                || strict_openai_chat_usage
                                || is_codex_route)
                                && sse_summary
                                    .as_ref()
                                    .is_some_and(|summary| !summary.usage_invalid))
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
            drop(terminal_frames);
            terminal_memory.set_bytes(0);
            drop(body_sender);
            terminal_delivery_phase.finish(
                transport_error.unwrap_or("returned"),
                Some(status.as_u16()),
                None,
            );
            let gap_response = format!("gap://{request_id}/response");
            let stored_response = gap_response.clone();
            let response_archive_attempt = None;
            let terminal_phase =
                proxy_diagnostics::Phase::new(diagnostic_context, "stream_terminal_settlement");
            finalize_streaming_lifecycle(StreamingFinalizationInput {
                output_timing,
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
                memory: request_memory,
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
            // This function handles its own database failures; returned means
            // it settled its work, not proof that persistence succeeded.
            terminal_phase.finish("returned", None, None);
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
        // The absolute proxy lifecycle ends after normal finalization or
        // timeout reconciliation. A slow archive tail remains memory-bounded
        // and connection-drain-owned, but must not retain scarce admission
        // concurrency past that boundary.
        drop(_proxy_lifecycle_permit);
        if let Err(error) = archive_eof_owner.await {
            tracing::error!(
                task_cancelled = error.is_cancelled(),
                task_panicked = error.is_panic(),
                "response archive EOF owner failed"
            );
        }
        stream_owner.finish("returned", Some(status.as_u16()), None);
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

fn chat_usage_capture(is_kimi_route: bool) -> ResponsesSseCapture {
    if is_kimi_route {
        ResponsesSseCapture::for_kimi_chat_usage()
    } else {
        ResponsesSseCapture::for_openai_chat_usage()
    }
}

async fn hold_response_eof_until_archive_settles(
    settlement: tokio::sync::oneshot::Receiver<
        Option<crate::response_archive_spool::ResponseArchiveSettlement>,
    >,
    _body_sender: tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    diagnostic_context: proxy_diagnostics::Context,
) {
    let handoff = proxy_diagnostics::Phase::new(diagnostic_context, "archive_terminal_handoff");
    if let Ok(Some(settlement)) = settlement.await {
        handoff.finish("accepted", None, None);
        let drain = proxy_diagnostics::Phase::new(diagnostic_context, "archive_eof_drain");
        settlement.wait().await;
        // wait() reports writer errors separately. Never label this success.
        drain.finish("returned", None, None);
    } else {
        handoff.finish("no_settlement", None, None);
    }
}
