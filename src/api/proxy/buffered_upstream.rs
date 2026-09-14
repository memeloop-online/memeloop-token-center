use super::*;

#[derive(Clone, Copy, Debug)]
pub(super) enum BoundedUpstreamError {
    ContentEncoding,
    MemoryCapacity,
    Timeout,
    ReadTimeout,
    RequestTimeout,
    ResponseTooLarge,
    Stream,
}

impl BoundedUpstreamError {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::ContentEncoding => "upstream_invalid_content_encoding",
            Self::MemoryCapacity => "upstream_response_memory_capacity",
            Self::Timeout => "upstream_timeout",
            Self::ReadTimeout => upstream_response::UPSTREAM_READ_TIMEOUT,
            Self::RequestTimeout => upstream_response::UPSTREAM_REQUEST_TIMEOUT,
            Self::ResponseTooLarge => "upstream_response_too_large",
            Self::Stream => "upstream_stream",
        }
    }

    fn from_stream_error(error: &str) -> Self {
        match error {
            upstream_response::UPSTREAM_READ_TIMEOUT => Self::ReadTimeout,
            upstream_response::UPSTREAM_REQUEST_TIMEOUT => Self::RequestTimeout,
            _ => Self::Stream,
        }
    }
}

pub(super) async fn read_bounded_upstream(
    response: UpstreamResponse,
    maximum: usize,
    memory: &crate::gateway_body::memory::ProxyMemoryReservation,
    started: Instant,
    reserve_adapter_maximum: bool,
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
    let declared_maximum = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(maximum)
        .min(maximum);
    let deadline =
        tokio::time::Instant::now() + MAX_PROXY_LIFETIME.saturating_sub(started.elapsed());
    let reservation_maximum = if reserve_adapter_maximum {
        maximum
    } else {
        declared_maximum
    };
    let diagnostic_context = proxy_diagnostics::Context::current();
    let capacity = proxy_diagnostics::Phase::new(diagnostic_context, "buffered_response_memory");
    if !memory
        .reserve_buffered_response(reservation_maximum, deadline)
        .await
    {
        capacity.finish("rejected", None, None);
        return Err(BoundedUpstreamError::MemoryCapacity);
    }
    capacity.finish("completed", None, None);
    let mut first_byte = Some(proxy_diagnostics::Phase::new(
        diagnostic_context,
        "buffered_first_byte",
    ));
    let maximum = declared_maximum;
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = tokio::time::timeout_at(deadline, stream.next())
        .await
        .map_err(|_| BoundedUpstreamError::Timeout)?
    {
        // Never retain or display reqwest's error: its URL can contain
        // credential-bearing upstream configuration.
        let chunk = chunk.map_err(BoundedUpstreamError::from_stream_error)?;
        if !chunk.is_empty()
            && let Some(phase) = first_byte.take()
        {
            phase.finish("received", None, Some(chunk.len()));
        }
        if body.len().saturating_add(chunk.len()) > maximum {
            return Err(BoundedUpstreamError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    if let Some(phase) = first_byte.take() {
        phase.finish("no_bytes", None, Some(0));
    }
    body.shrink_to_fit();
    if !memory.response_json_fits(&body) {
        return Err(BoundedUpstreamError::MemoryCapacity);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let result =
                read_bounded_upstream(response, 1024, &memory, Instant::now(), false).await;
            assert_eq!(result.unwrap_err().code(), expected);
        }
    }
}
