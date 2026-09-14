use super::*;

#[derive(Clone, Copy, Debug)]
pub(super) enum BoundedUpstreamError {
    ContentEncoding,
    MemoryCapacity,
    Timeout,
    ResponseTooLarge,
    Stream,
}

impl BoundedUpstreamError {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::ContentEncoding => "upstream_invalid_content_encoding",
            Self::MemoryCapacity => "upstream_response_memory_capacity",
            Self::Timeout => "upstream_timeout",
            Self::ResponseTooLarge => "upstream_response_too_large",
            Self::Stream => "upstream_stream",
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
    if !memory
        .reserve_buffered_response(reservation_maximum, deadline)
        .await
    {
        return Err(BoundedUpstreamError::MemoryCapacity);
    }
    let maximum = declared_maximum;
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = tokio::time::timeout_at(deadline, stream.next())
        .await
        .map_err(|_| BoundedUpstreamError::Timeout)?
    {
        // Never retain or display reqwest's error: its URL can contain
        // credential-bearing upstream configuration.
        let chunk = chunk.map_err(|_| BoundedUpstreamError::Stream)?;
        if body.len().saturating_add(chunk.len()) > maximum {
            return Err(BoundedUpstreamError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    body.shrink_to_fit();
    if !memory.response_json_fits(&body) {
        return Err(BoundedUpstreamError::MemoryCapacity);
    }
    Ok(body)
}
