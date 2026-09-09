use super::*;

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
