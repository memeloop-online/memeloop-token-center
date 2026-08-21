use super::*;

struct AbortTaskOnDrop<T>(Option<tokio::task::JoinHandle<T>>);

impl<T> AbortTaskOnDrop<T> {
    fn new(task: tokio::task::JoinHandle<T>) -> Self {
        Self(Some(task))
    }

    fn abort(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

impl<T> Drop for AbortTaskOnDrop<T> {
    fn drop(&mut self) {
        self.abort();
    }
}

async fn run_bounded_proxy_lifecycle<F>(
    deadline: tokio::time::Instant,
    lifecycle: F,
) -> Result<F::Output, tokio::time::error::Elapsed>
where
    F: std::future::Future,
{
    tokio::time::timeout_at(deadline, lifecycle).await
}

pub(super) struct StreamingResponse<'a> {
    pub(super) state: &'a AppState,
    pub(super) upstream: reqwest::Response,
    pub(super) status: StatusCode,
    pub(super) content_type: Option<HeaderValue>,
    pub(super) is_sse: bool,
    pub(super) capture_json_usage: bool,
    pub(super) protocol: Protocol,
    pub(super) is_codex_route: bool,
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
        request_id,
        buffered_request,
        proxy_lifecycle_permit,
    } = input;
    let mut response_archive_attempt =
        match begin_proxy_archive_attempt(&state.db, request_id, ArchiveStagingPurpose::Response)
            .await
        {
            Ok(attempt) => Some(attempt),
            Err(_) => {
                tracing::warn!(%request_id, stage = "response_archive_begin", "proxy archive gap");
                None
            }
        };
    let archive_writer = if let Some(attempt) = response_archive_attempt.as_ref() {
        match state.archive.start_writer(&attempt.object_locator).await {
            Ok(writer) => Some(writer),
            Err(_) => {
                abandon_proxy_archive_attempt(&state.db, attempt).await;
                response_archive_attempt = None;
                tracing::warn!(%request_id, stage = "response_archive", "proxy archive gap");
                None
            }
        }
    } else {
        None
    };
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
        let lifecycle_started = tokio::time::Instant::now();
        let stream_deadline = lifecycle_started + MAX_PROXY_STREAM_LIFETIME;
        let lifecycle_deadline = lifecycle_started + MAX_PROXY_LIFETIME;
        let lifecycle = async move {
        let mut upstream_stream = upstream.bytes_stream();
        let mut archive_writer = archive_writer;
        let mut response_archive_attempt = response_archive_attempt;
        let mut usage_capture = Vec::new();
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
        let (archive_lease_lost_sender, mut archive_lease_lost_receiver) =
            tokio::sync::mpsc::channel(1);
        let archive_heartbeat_task = response_archive_attempt.clone().map(|mut attempt| {
            let heartbeat_database = background_state.db.clone();
            AbortTaskOnDrop::new(tokio::spawn(async move {
                let mut heartbeat = tokio::time::interval(Duration::from_millis(
                    u64::try_from(ARCHIVE_STAGING_WRITE_HEARTBEAT_MILLIS).unwrap_or(20_000),
                ));
                heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                heartbeat.tick().await;
                loop {
                    heartbeat.tick().await;
                    if !heartbeat_proxy_archive_attempt(&heartbeat_database, &mut attempt)
                        .await
                        .unwrap_or(false)
                    {
                        let _ = archive_lease_lost_sender.send(()).await;
                        break;
                    }
                }
            }))
        });
        loop {
            let polled = tokio::select! {
                biased;
                _ = archive_lease_lost_receiver.recv(), if response_archive_attempt.is_some() => None,
                next = tokio::time::timeout_at(stream_deadline, upstream_stream.next()) => Some(next),
            };
            let Some(polled) = polled else {
                if let Some(writer) = archive_writer.take() {
                    drop(tokio::spawn(async move {
                        let _ = writer.abort().await;
                    }));
                }
                response_archive_attempt = None;
                tracing::warn!(%request_id, stage = "response_archive_heartbeat", "proxy archive lease lost; downstream response continues with an archive gap");
                continue;
            };
            let next = match polled {
                Ok(next) => next,
                Err(_) => {
                    transport_error = Some("upstream_timeout");
                    if let Some(writer) = archive_writer.take() {
                        let _ = writer.abort().await;
                    }
                    let _ = tokio::time::timeout(
                        MAX_DOWNSTREAM_SEND_WAIT,
                        body_sender.send(Err(std::io::Error::other("upstream stream timed out"))),
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
                    response_bytes = response_bytes.saturating_add(raw_chunk.len());
                    if response_bytes > MAX_PROXY_RESPONSE_BODY {
                        transport_error = Some("upstream_response_too_large");
                        if let Some(writer) = archive_writer.take() {
                            let _ = writer.abort().await;
                        }
                        let _ = tokio::time::timeout(
                            MAX_DOWNSTREAM_SEND_WAIT,
                            body_sender.send(Err(std::io::Error::other(
                                "upstream response exceeded the size limit",
                            ))),
                        )
                        .await;
                        break;
                    }
                    let (chunk, chunk_billable) =
                        if let Some(sanitizer) = responses_streaming_sanitizer.as_mut() {
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
                                    if let Some(writer) = archive_writer.take() {
                                        let _ = writer.abort().await;
                                    }
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
                    }
                    if let Some(capture) = sse_capture.as_mut() {
                        capture.push(&chunk);
                    }
                    if let Some(mut writer) = archive_writer.take() {
                        match writer.write(chunk.clone()).await {
                            Ok(()) => archive_writer = Some(writer),
                            Err(_) => {
                                tracing::warn!(%request_id, stage = "response_archive_stream", "proxy archive gap");
                                let _ = writer.abort().await;
                                if let Some(attempt) = response_archive_attempt.take() {
                                    abandon_proxy_archive_attempt(&background_state.db, &attempt)
                                        .await;
                                }
                            }
                        }
                    }
                    if chunk.is_empty() {
                        continue;
                    }
                    if !delivered_any {
                        match tokio::time::timeout(MAX_DOWNSTREAM_SEND_WAIT, body_sender.reserve())
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
                                                body_sender
                                                    .send(Ok::<Bytes, std::io::Error>(remaining)),
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
                        if let Some(writer) = archive_writer.take() {
                            let _ = writer.abort().await;
                        }
                        break;
                    }
                }
                Err(_) => {
                    transport_error = Some("upstream_stream");
                    if let Some(writer) = archive_writer.take() {
                        let _ = writer.abort().await;
                    }
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
            if let Some(writer) = archive_writer.take() {
                let _ = writer.abort().await;
            }
            if let Some(attempt) = response_archive_attempt.take() {
                abandon_proxy_archive_attempt(&background_state.db, &attempt).await;
            }
        }
        let sse_summary = sse_capture.map(ResponsesSseCapture::finish_summary);
        let stored_response = if transport_error.is_none()
            && let Some(writer) = archive_writer
        {
            match writer.finish_staged().await {
                Ok(staged)
                    if response_archive_attempt
                        .as_ref()
                        .is_some_and(|attempt| attempt.object_locator == staged.object_locator) =>
                {
                    staged.object_locator
                }
                Ok(_) | Err(_) => {
                    if let Some(attempt) = response_archive_attempt.take() {
                        abandon_proxy_archive_attempt(&background_state.db, &attempt).await;
                    }
                    tracing::warn!(%request_id, stage = "response_archive_finish", "proxy archive gap");
                    format!("gap://{request_id}/response")
                }
            }
        } else {
            format!("gap://{request_id}/response")
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
        let conversation_input = conversation
            .as_ref()
            .map(|conversation| ProxyConversationInput {
                key: &conversation.key,
                request_json: &conversation.request_json,
                hints: &conversation.hints,
                client_name: conversation.client_name.as_deref(),
                upstream_response_id: response_id.as_deref(),
            });
        let terminal_result = finish_proxy_request_with_retry(
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
        )
        .await;
        if response_archive_requires_cleanup(&terminal_result, &stored_response)
            && let Some(attempt) = response_archive_attempt.as_ref()
        {
            abandon_proxy_archive_attempt(&background_state.db, attempt).await;
        }
        if let Some(mut task) = archive_heartbeat_task {
            task.abort();
        }
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
