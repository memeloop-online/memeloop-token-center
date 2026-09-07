use super::*;

/// One upstream network chunk can contain several fully-framed SSE events.
/// Keep those immutable slices together so a capacity-one archive channel
/// cannot mistake intra-chunk framing for archive backpressure.
pub(super) struct ResponseArchiveBatch {
    pub(super) chunks: Vec<Bytes>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ResponseArchiveBatchError {
    BatchLimit,
    Backpressure,
}

pub(super) struct CapturedSseDelivery {
    pub(super) frames: Vec<SseDeliveryFrame>,
    pub(super) strict_chat_terminal_ready: bool,
}

pub(super) fn downstream_stream_failure(
    protocol: Protocol,
    is_sse: bool,
    sanitizer: Option<&crate::api::sse::ResponsesStreamingSanitizer>,
    message: &'static str,
) -> Result<Bytes, std::io::Error> {
    // Once a Responses stream has been admitted, returning an error item from
    // Body::from_stream resets the HTTP body. Responses clients surface that
    // as a transport/body-decode failure instead of a terminal API error.
    // Keep other streaming protocols' existing error behavior unchanged.
    if is_sse && matches!(protocol, Protocol::OpenAiResponses) {
        if sanitizer.is_some_and(crate::api::sse::ResponsesStreamingSanitizer::has_failed_terminal)
        {
            Ok(Bytes::new())
        } else {
            Ok(crate::api::sse::safe_failure_event())
        }
    } else {
        Err(std::io::Error::other(message))
    }
}

pub(super) fn capture_sse_delivery(
    capture: Option<&mut ResponsesSseCapture>,
    chunk: Bytes,
    strict_openai_chat_usage: bool,
) -> Result<CapturedSseDelivery, crate::api::sse::SseFramerRejection> {
    let Some(capture) = capture else {
        return Ok(CapturedSseDelivery {
            frames: vec![SseDeliveryFrame {
                bytes: chunk,
                billable: true,
            }],
            strict_chat_terminal_ready: false,
        });
    };
    let frames = capture.push_delivery_frames(&chunk)?;
    Ok(CapturedSseDelivery {
        frames,
        strict_chat_terminal_ready: strict_openai_chat_usage
            && capture.strict_chat_terminal_ready(),
    })
}

impl ResponseArchiveBatch {
    pub(super) fn from_delivery_frames(
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

#[cfg(test)]
pub(super) fn try_queue_response_archive_batch(
    sender: &tokio::sync::mpsc::Sender<ResponseArchiveBatch>,
    frames: &[SseDeliveryFrame],
) -> Result<(), ResponseArchiveBatchError> {
    let Some(batch) = ResponseArchiveBatch::from_delivery_frames(frames)? else {
        return Ok(());
    };
    try_send_response_archive_batch(sender, batch)
}

pub(super) fn try_send_response_archive_batch(
    sender: &tokio::sync::mpsc::Sender<ResponseArchiveBatch>,
    batch: ResponseArchiveBatch,
) -> Result<(), ResponseArchiveBatchError> {
    sender
        .try_send(batch)
        .map_err(|_| ResponseArchiveBatchError::Backpressure)
}
