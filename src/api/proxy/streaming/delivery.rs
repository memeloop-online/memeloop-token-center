use super::*;

pub(super) struct CapturedSseDelivery {
    pub(super) frames: Vec<SseDeliveryFrame>,
    pub(super) strict_chat_terminal_ready: bool,
}

pub(super) struct FrameDelivery<'a> {
    pub waits: &'a mut wait_diagnostics::WaitDiagnostics,
    pub sender: &'a tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    pub queued: &'a mut bool,
    pub probe: Option<&'a mut UpstreamAttemptGuard>,
}

pub(super) async fn send_frame(
    input: FrameDelivery<'_>,
    frame: SseDeliveryFrame,
) -> Result<bool, &'static str> {
    let SseDeliveryFrame {
        bytes, billable, ..
    } = frame;
    if billable && !*input.queued {
        let permit = input
            .waits
            .downstream(tokio::time::timeout(
                MAX_DOWNSTREAM_SEND_WAIT,
                input.sender.reserve(),
            ))
            .await
            .map_err(|_| "downstream_backpressure")?
            .map_err(|_| "downstream_disconnected")?;
        permit.send(Ok(bytes));
        *input.queued = true;
    } else {
        input
            .waits
            .downstream(tokio::time::timeout(
                MAX_DOWNSTREAM_SEND_WAIT,
                input.sender.send(Ok(bytes)),
            ))
            .await
            .map_err(|_| "downstream_backpressure")?
            .map_err(|_| "downstream_disconnected")?;
    }
    if billable && let Some(probe) = input.probe {
        probe.delivered_validated_output().await;
    }
    Ok(billable)
}

/// Keep only the bounded terminal tail, never the streamed response body.
pub(super) struct TerminalFrames {
    frames: Vec<SseDeliveryFrame>,
    bytes: usize,
    limit_bytes: usize,
}

impl TerminalFrames {
    pub(super) fn with_limit(limit_bytes: usize) -> Self {
        Self {
            frames: Vec::new(),
            bytes: 0,
            limit_bytes,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
    pub(super) fn hold(&mut self, frame: SseDeliveryFrame) -> Result<Option<SseDeliveryFrame>, ()> {
        if !frame.terminal && self.frames.is_empty() {
            return Ok(Some(frame));
        }
        self.bytes = self.bytes.checked_add(frame.bytes.len()).ok_or(())?;
        if self.bytes > self.limit_bytes {
            return Err(());
        }
        self.frames.push(frame);
        Ok(None)
    }

    pub(super) fn take(&mut self) -> Vec<SseDeliveryFrame> {
        self.bytes = 0;
        std::mem::take(&mut self.frames)
    }

    pub(super) fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Default for TerminalFrames {
    fn default() -> Self {
        Self::with_limit(crate::provider::SseFramingLimits::DEFAULT_TERMINAL_HOLD_BYTES)
    }
}

pub(super) fn invalid_terminal_failure(protocol: Protocol) -> Bytes {
    match protocol {
        Protocol::OpenAiResponses | Protocol::OpenAiResponsesCompact => {
            crate::api::sse::safe_failure_event()
        }
        Protocol::AnthropicMessages => Bytes::from_static(
            b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"upstream stream did not complete\"}}\n\n",
        ),
        _ => Bytes::from_static(
            b"data: {\"error\":{\"type\":\"upstream_error\",\"message\":\"upstream stream did not complete\"}}\n\n",
        ),
    }
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
    if is_sse
        && matches!(
            protocol,
            Protocol::OpenAiResponses | Protocol::OpenAiResponsesCompact
        )
    {
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
                terminal: false,
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

#[cfg(test)]
#[path = "delivery_log_tests.rs"]
mod delivery_log_tests;
