use super::*;

#[derive(Clone, Copy, Debug)]
pub(super) enum BoundedUpstreamError {
    ContentEncoding,
    MemoryCapacity,
    ReadTimeout,
    RequestTimeout,
    ResponseTooLarge,
    Stream,
    Http2Reset,
    Http2GoAway,
}

impl BoundedUpstreamError {
    pub(super) const fn terminal_cause(self) -> Option<crate::model::RequestTerminalCause> {
        use crate::model::RequestTerminalCause;
        match self {
            Self::ReadTimeout => Some(RequestTerminalCause::ReadTimeout),
            Self::RequestTimeout => Some(RequestTerminalCause::RequestTimeout),
            Self::Stream => Some(RequestTerminalCause::StreamReadError),
            Self::Http2Reset => Some(RequestTerminalCause::Http2Reset),
            Self::Http2GoAway => Some(RequestTerminalCause::Http2GoAway),
            _ => None,
        }
    }

    pub(super) fn code(self) -> &'static str {
        match self {
            Self::ContentEncoding => "upstream_invalid_content_encoding",
            Self::MemoryCapacity => "upstream_response_memory_capacity",
            Self::ReadTimeout => upstream_response::UPSTREAM_READ_TIMEOUT,
            Self::RequestTimeout => upstream_response::UPSTREAM_REQUEST_TIMEOUT,
            Self::ResponseTooLarge => "upstream_response_too_large",
            Self::Stream => "upstream_stream",
            Self::Http2Reset => upstream_response::UPSTREAM_HTTP2_RESET,
            Self::Http2GoAway => upstream_response::UPSTREAM_HTTP2_GOAWAY,
        }
    }

    fn from_stream_error(error: &str) -> Self {
        match error {
            upstream_response::UPSTREAM_READ_TIMEOUT => Self::ReadTimeout,
            upstream_response::UPSTREAM_REQUEST_TIMEOUT => Self::RequestTimeout,
            upstream_response::UPSTREAM_HTTP2_RESET => Self::Http2Reset,
            upstream_response::UPSTREAM_HTTP2_GOAWAY => Self::Http2GoAway,
            _ => Self::Stream,
        }
    }
}

pub(super) fn upstream_error_health_terminal(error_code: &str) -> UpstreamAttemptTerminal {
    if is_local_response_boundary(error_code)
        || matches!(
            error_code,
            "upstream_timeout"
                | "upstream_read_timeout"
                | "upstream_request_timeout"
                | "upstream_stream"
                | "upstream_stream_read_error"
                | "upstream_http2_reset"
                | "upstream_http2_goaway"
        )
    {
        UpstreamAttemptTerminal::Inconclusive
    } else {
        UpstreamAttemptTerminal::invalid_response()
    }
}

pub(super) fn is_local_response_boundary(error_code: &str) -> bool {
    matches!(
        error_code,
        "upstream_response_event_too_large"
            | "upstream_response_event_batch_too_large"
            | "upstream_response_terminal_too_large"
            | "upstream_response_too_large"
            | "upstream_response_memory_capacity"
            | "upstream_unsupported_chat_output"
    )
}

struct ResponseReadMemory<'a> {
    memory: &'a crate::gateway_body::memory::ProxyMemoryReservation,
    conversation: Option<&'a ProxyConversation>,
    keep: bool,
}

impl Drop for ResponseReadMemory<'_> {
    fn drop(&mut self) {
        if !self.keep {
            self.memory.release_buffered_response();
            if let Some(conversation) = self.conversation {
                conversation.reject_projection();
            }
        }
    }
}

pub(super) async fn read_bounded_upstream(
    response: UpstreamResponse,
    maximum: usize,
    memory: &crate::gateway_body::memory::ProxyMemoryReservation,
    memory_admission_wait: Duration,
    reserve_adapter_maximum: bool,
    conversation: Option<&ProxyConversation>,
) -> Result<Vec<u8>, BoundedUpstreamError> {
    if response
        .headers()
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .any(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
    {
        return Err(BoundedUpstreamError::ContentEncoding);
    }
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(BoundedUpstreamError::ResponseTooLarge);
    }
    let has_content_length = response.content_length().is_some();
    let declared_maximum = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(maximum)
        .min(maximum);
    let deadline = tokio::time::Instant::now() + memory_admission_wait;
    let diagnostic_context = proxy_diagnostics::Context::current();
    let capacity = proxy_diagnostics::Phase::new(diagnostic_context, "buffered_response_memory");
    let reserved = if let Some(conversation) = conversation {
        conversation.reserve_for_buffered_response(memory, declared_maximum, deadline)
    } else {
        memory.reserve_buffered_response(declared_maximum, deadline)
    };
    if !reserved {
        tracing::warn!(request_id = %diagnostic_context.request_id,
            stage = "buffered_response_admission", reserved_bytes = memory.buffered_response_reserved_bytes(),
            actual_bytes = 0, has_content_length, "buffered response memory unavailable");
        capacity.finish("rejected", None, None);
        return Err(BoundedUpstreamError::MemoryCapacity);
    }
    let mut read_memory = ResponseReadMemory {
        memory,
        conversation,
        keep: false,
    };
    capacity.finish("completed", None, None);
    let mut first_byte = Some(proxy_diagnostics::Phase::new(
        diagnostic_context,
        "buffered_first_byte",
    ));
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        // Never retain or display reqwest's error: its URL can contain
        // credential-bearing upstream configuration.
        let chunk = chunk.map_err(BoundedUpstreamError::from_stream_error)?;
        if !chunk.is_empty()
            && let Some(phase) = first_byte.take()
        {
            phase.finish("received", None, Some(chunk.len()));
        }
        let required = body.len().saturating_add(chunk.len());
        if required > declared_maximum {
            return Err(BoundedUpstreamError::ResponseTooLarge);
        }
        if required > body.capacity() {
            let capacity = required
                .max(body.capacity().saturating_mul(2))
                .min(declared_maximum);
            if !memory.grow_buffered_response(capacity) {
                tracing::warn!(request_id = %diagnostic_context.request_id,
                    stage = "buffered_response_chunk", reserved_bytes = memory.buffered_response_reserved_bytes(),
                    actual_bytes = required, has_content_length, "buffered response growth unavailable");
                return Err(BoundedUpstreamError::MemoryCapacity);
            }
            body.try_reserve_exact(capacity - body.len())
                .map_err(|_| BoundedUpstreamError::MemoryCapacity)?;
        }
        body.extend_from_slice(&chunk);
    }
    if let Some(phase) = first_byte.take() {
        phase.finish("no_bytes", None, Some(0));
    }
    body.shrink_to_fit();
    if !memory.reserve_response_json(&body) {
        tracing::warn!(request_id = %diagnostic_context.request_id,
            stage = "buffered_response_json", reserved_bytes = memory.buffered_response_reserved_bytes(),
            actual_bytes = body.len(), has_content_length, "buffered response JSON memory unavailable");
        return Err(BoundedUpstreamError::MemoryCapacity);
    }
    memory.shrink_buffered_response(&body);
    if reserve_adapter_maximum && !memory.grow_buffered_response(maximum) {
        tracing::warn!(request_id = %diagnostic_context.request_id,
            stage = "buffered_response_adapter", reserved_bytes = memory.buffered_response_reserved_bytes(),
            actual_bytes = body.len(), has_content_length, "component working memory unavailable");
        return Err(BoundedUpstreamError::MemoryCapacity);
    }
    tracing::debug!(request_id = %diagnostic_context.request_id,
        stage = "buffered_response_ready", reserved_bytes = memory.buffered_response_reserved_bytes(),
        actual_bytes = body.len(), has_content_length, "buffered response memory accounted");
    read_memory.keep = true;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dense_small_json_succeeds_with_declared_or_chunked_body_at_low_global_usage() {
        let body = Bytes::from(format!("[{}0]", "0,".repeat(299)));
        for content_length in [Some(body.len() as u64), None] {
            let budget =
                crate::gateway_body::memory::ProxyMemoryBudget::new(2 * 1024 * 1024 * 1024);
            let other = budget.reservation();
            assert!(other.try_grow(70 * 1024 * 1024, 1));
            let baseline = budget.snapshot().0;
            let memory = budget.reservation();
            let response = UpstreamResponse::Prefetched {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                version: http::Version::HTTP_11,
                content_length,
                stream: Box::pin(futures_util::stream::iter(
                    body.chunks(11)
                        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                        .collect::<Vec<_>>(),
                )),
            };
            let result = read_bounded_upstream(
                response,
                MAX_PROXY_RESPONSE_BODY,
                &memory,
                Duration::from_secs(30),
                false,
                None,
            )
            .await
            .unwrap();
            assert_eq!(result, body);
            assert!(memory.response_json_fits(&body));
            assert_eq!(memory.buffered_response_reserved_bytes(), 128 * 1024);
            memory.release_buffered_response();
            assert_eq!(budget.snapshot().0, baseline);
        }
    }

    #[tokio::test]
    async fn reader_errors_and_cancellation_refund_response_with_request_owner_alive() {
        let budget = crate::gateway_body::memory::ProxyMemoryBudget::new(1024 * 1024);
        let memory = budget.reservation();
        assert!(memory.try_grow(1234, 1));
        let baseline = budget.snapshot();
        for chunk in [Ok(Bytes::from_static(b"too big")), Err("upstream_stream")] {
            let response = UpstreamResponse::Prefetched {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                version: http::Version::HTTP_11,
                content_length: Some(2),
                stream: Box::pin(futures_util::stream::iter([chunk])),
            };
            assert!(
                read_bounded_upstream(
                    response,
                    1024,
                    &memory,
                    Duration::from_secs(30),
                    false,
                    None
                )
                .await
                .is_err()
            );
            assert_eq!(budget.snapshot(), baseline);
        }
        let owned = memory.clone();
        let (started, waiting) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let response = UpstreamResponse::Prefetched {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                version: http::Version::HTTP_11,
                content_length: None,
                stream: Box::pin(futures_util::stream::once(async move {
                    let _ = started.send(());
                    futures_util::future::pending().await
                })),
            };
            read_bounded_upstream(response, 1024, &owned, Duration::from_secs(30), false, None)
                .await
        });
        waiting.await.unwrap();
        assert_eq!(budget.snapshot().0, baseline.0 + 64 * 1024);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(budget.snapshot(), baseline);
    }

    #[tokio::test]
    async fn dense_response_global_shortage_releases_partial_buffer() {
        let budget = crate::gateway_body::memory::ProxyMemoryBudget::new(128 * 1024);
        let memory = budget.reservation();
        assert!(memory.try_grow(1234, 1));
        let baseline = budget.snapshot();
        let response = UpstreamResponse::Prefetched {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            version: http::Version::HTTP_11,
            content_length: Some(601),
            stream: Box::pin(futures_util::stream::iter([Ok(Bytes::from(format!(
                "[{}0]",
                "0,".repeat(299)
            )))])),
        };
        assert!(matches!(
            read_bounded_upstream(
                response,
                MAX_PROXY_RESPONSE_BODY,
                &memory,
                Duration::from_secs(30),
                false,
                None
            )
            .await,
            Err(BoundedUpstreamError::MemoryCapacity)
        ));
        assert_eq!(budget.snapshot(), baseline);
    }

    #[tokio::test(start_paused = true)]
    async fn buffered_body_outlives_twenty_minutes_without_relaxing_byte_admission() {
        let response = UpstreamResponse::Prefetched {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            version: http::Version::HTTP_11,
            content_length: Some(2),
            stream: Box::pin(futures_util::stream::once(async {
                tokio::time::sleep(Duration::from_secs(1500)).await;
                Ok(Bytes::from_static(b"{}"))
            })),
        };
        let budget = crate::gateway_body::memory::ProxyMemoryBudget::new(
            crate::config::DEFAULT_PROXY_MEMORY_BUDGET_BYTES,
        );
        let memory = budget.reservation();
        let started = tokio::time::Instant::now();
        assert_eq!(
            read_bounded_upstream(
                response,
                1024,
                &memory,
                Duration::from_secs(30),
                false,
                None
            )
            .await
            .unwrap(),
            b"{}"
        );
        assert_eq!(started.elapsed(), Duration::from_secs(1500));
        drop(memory);
        assert_eq!(budget.snapshot().0, 0);
    }

    #[test]
    fn local_response_boundaries_are_inconclusive_health_evidence() {
        for error in [
            "upstream_response_event_too_large",
            "upstream_response_event_batch_too_large",
            "upstream_response_terminal_too_large",
            "upstream_response_too_large",
            "upstream_response_memory_capacity",
            "upstream_unsupported_chat_output",
        ] {
            assert!(matches!(
                upstream_error_health_terminal(error),
                UpstreamAttemptTerminal::Inconclusive
            ));
        }
        assert!(matches!(
            upstream_error_health_terminal("upstream_invalid_content_type"),
            UpstreamAttemptTerminal::Failed { .. }
        ));
    }

    #[tokio::test]
    async fn unexpected_json_rejects_memory_before_polling_any_body_bytes() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let budget = Arc::new(crate::gateway_body::memory::ProxyMemoryBudget::new(
            1024 * 1024,
        ));
        let held = budget.reservation();
        assert!(held.try_grow(1024 * 1024, 1));
        let polled = Arc::new(AtomicBool::new(false));
        let observed = polled.clone();
        let response = UpstreamResponse::Prefetched {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            version: http::Version::HTTP_11,
            content_length: Some(2),
            stream: Box::pin(futures_util::stream::once(async move {
                observed.store(true, Ordering::SeqCst);
                Ok(Bytes::from_static(b"{}"))
            })),
        };
        let memory = budget.reservation();
        let rejected = read_bounded_upstream(
            response,
            1024,
            &memory,
            Duration::from_secs(30),
            false,
            None,
        )
        .await;
        assert!(matches!(
            rejected,
            Err(BoundedUpstreamError::MemoryCapacity)
        ));
        assert!(!polled.load(Ordering::SeqCst));
        assert_eq!(budget.snapshot().0, 1024 * 1024);
        drop(held);
        drop(memory);
        assert_eq!(budget.snapshot().0, 0);

        let polled = Arc::new(AtomicBool::new(false));
        let observed = polled.clone();
        let response = UpstreamResponse::Prefetched {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            version: http::Version::HTTP_11,
            content_length: Some(2),
            stream: Box::pin(futures_util::stream::once(async move {
                observed.store(true, Ordering::SeqCst);
                Ok(Bytes::from_static(b"{}"))
            })),
        };
        let memory = budget.reservation();
        assert_eq!(
            read_bounded_upstream(
                response,
                1024,
                &memory,
                Duration::from_secs(30),
                false,
                None,
            )
            .await
            .unwrap(),
            b"{}"
        );
        assert!(polled.load(Ordering::SeqCst));
        drop(memory);
        assert_eq!(budget.snapshot().0, 0);
    }

    #[tokio::test]
    async fn buffered_reader_preserves_only_allowlisted_transport_timeouts() {
        for (upstream_error, expected) in [
            (
                upstream_response::UPSTREAM_READ_TIMEOUT,
                "upstream_read_timeout",
            ),
            (
                upstream_response::UPSTREAM_REQUEST_TIMEOUT,
                "upstream_request_timeout",
            ),
            (
                upstream_response::UPSTREAM_HTTP2_RESET,
                "upstream_http2_reset",
            ),
            (
                upstream_response::UPSTREAM_HTTP2_GOAWAY,
                "upstream_http2_goaway",
            ),
            ("SECRET_PROVIDER_ERROR_CANARY", "upstream_stream"),
        ] {
            let response = UpstreamResponse::Prefetched {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                version: http::Version::HTTP_11,
                content_length: None,
                stream: Box::pin(futures_util::stream::iter([Err(upstream_error)])),
            };
            let budget = crate::gateway_body::memory::ProxyMemoryBudget::new(
                crate::config::DEFAULT_PROXY_MEMORY_BUDGET_BYTES,
            );
            let memory = budget.reservation();
            let result = read_bounded_upstream(
                response,
                1024,
                &memory,
                Duration::from_secs(30),
                false,
                None,
            )
            .await;
            assert_eq!(result.unwrap_err().code(), expected);
        }
    }
}
