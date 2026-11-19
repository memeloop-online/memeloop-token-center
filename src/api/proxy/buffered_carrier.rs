//! Durable response capture separates upstream ownership from JSON conversion.
use super::buffered_upstream::BoundedUpstreamError;
use super::*;
use crate::gateway_body::carrier::{BodyCarrier, CarrierCapture};

pub(super) async fn read(
    response: UpstreamResponse,
    maximum: usize,
    request: &BufferedRequest<'_>,
    state: &AppState,
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
    let staging = crate::archive_staging::ArchiveStagingKey::new(
        crate::archive_staging::ArchiveStagingOwner::ProxyRequest(request.request_id),
        crate::archive_staging::ArchiveStagingPurpose::Response,
        Uuid::now_v7(),
    )
    .map_err(|_| BoundedUpstreamError::Stream)?;
    let mut scanner = crate::gateway_body::memory::JsonMemoryScanner::default();
    let mut length = 0usize;
    let mut failure = None;
    let stream = response.bytes_stream().map(|chunk| {
        let chunk = chunk.map_err(|code| {
            failure = Some(BoundedUpstreamError::from_stream_error(code));
            "upstream_stream"
        })?;
        length = length.saturating_add(chunk.len());
        if length > maximum {
            failure = Some(BoundedUpstreamError::ResponseTooLarge);
            return Err("upstream_response_too_large");
        }
        scanner.observe(&chunk);
        Ok(chunk)
    });
    let source = BodyCarrier::capture(
        stream,
        CarrierCapture::new(
            state.proxy_memory_budget.clone(),
            Some(state.archive.clone()),
            staging,
        ),
    )
    .await;
    let source = source.map_err(|_| failure.unwrap_or(BoundedUpstreamError::Stream))?;
    // Offload all source allocations before acquiring conversion credit: no
    // converter waits while retaining its own partial response in RAM.
    source
        .seal_to_objects()
        .await
        .map_err(|_| BoundedUpstreamError::Stream)?;
    let memory = &request.memory;
    let deadline = tokio::time::Instant::now() + request.memory_admission_wait;
    let nodes = scanner.observe(&[]);
    let reserved = if let Some(conversation) = request.conversation.as_ref() {
        conversation
            .reserve_for_buffered_shape(memory, length, nodes, deadline)
            .await
    } else {
        memory
            .reserve_buffered_shape_with_projection(length, nodes, 0, 1, deadline)
            .await
            .is_some()
    };
    if !reserved {
        return Err(BoundedUpstreamError::MemoryCapacity);
    }
    let mut guard = super::buffered_upstream::ResponseReadMemory {
        memory,
        conversation: request.conversation.as_ref(),
        keep: false,
    };
    let mut body = Vec::new();
    body.try_reserve_exact(length)
        .map_err(|_| BoundedUpstreamError::MemoryCapacity)?;
    body.resize(length, 0);
    source
        .read_into(0, &mut body)
        .await
        .map_err(|_| BoundedUpstreamError::Stream)?;
    guard.keep = true;
    Ok(body)
}
