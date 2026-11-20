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
    read_captured(
        response,
        maximum,
        ResponseCaptureContext {
            memory: &request.memory,
            budget: state.proxy_memory_budget.clone(),
            archive: state.archive.clone(),
            request_id: request.request_id,
            conversation: request.conversation.as_ref(),
            wait: request.memory_admission_wait,
            #[cfg(test)]
            conversion_ready: None,
        },
    )
    .await
}

struct ResponseCaptureContext<'a> {
    memory: &'a crate::gateway_body::memory::ProxyMemoryReservation,
    budget: std::sync::Arc<crate::gateway_body::memory::ProxyMemoryBudget>,
    archive: crate::archive::ArchiveStore,
    request_id: Uuid,
    conversation: Option<&'a ProxyConversation>,
    wait: Duration,
    #[cfg(test)]
    conversion_ready: Option<std::sync::Arc<tokio::sync::Notify>>,
}

async fn read_captured(
    response: UpstreamResponse,
    maximum: usize,
    context: ResponseCaptureContext<'_>,
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
        crate::archive_staging::ArchiveStagingOwner::ProxyRequest(context.request_id),
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
        CarrierCapture::new(context.budget, Some(context.archive), staging),
    )
    .await;
    let source = source.map_err(|_| failure.unwrap_or(BoundedUpstreamError::Stream))?;
    // Offload all source allocations before acquiring conversion credit: no
    // converter waits while retaining its own partial response in RAM.
    source
        .seal_to_objects()
        .await
        .map_err(|_| BoundedUpstreamError::Stream)?;
    #[cfg(test)]
    if let Some(ready) = &context.conversion_ready {
        ready.notify_one();
    }
    let memory = context.memory;
    let deadline = tokio::time::Instant::now() + context.wait;
    let nodes = scanner.observe(&[]);
    let reserved = if let Some(conversation) = context.conversation {
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
        conversation: context.conversation,
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

#[cfg(test)]
#[path = "buffered_carrier_tests.rs"]
mod tests;
