use super::*;

pub(super) struct CapturedSseDelivery {
    pub(super) frames: Vec<SseDeliveryFrame>,
    pub(super) strict_chat_terminal_ready: bool,
}

pub(super) struct FrameDelivery<'a> {
    pub state: &'a AppState,
    pub sender: &'a tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    pub request_id: Uuid,
    pub tenant_id: Uuid,
    pub reservation: &'a crate::model::UsageReservation,
    pub input_token_ceiling: i64,
    pub output_token_ceiling: i64,
    pub requested_service_tier: Option<&'a str>,
    pub confirmed: &'a mut bool,
    pub probe: Option<&'a mut UpstreamAttemptGuard>,
}

/// The held terminal and ordinary frames use exactly the same durable
/// delivery-start transition, including a terminal containing the only output.
pub(super) async fn send_frame(
    input: FrameDelivery<'_>,
    frame: SseDeliveryFrame,
) -> Result<bool, &'static str> {
    let SseDeliveryFrame {
        bytes, billable, ..
    } = frame;
    if billable && !*input.confirmed {
        let permit = tokio::time::timeout(MAX_DOWNSTREAM_SEND_WAIT, input.sender.reserve())
            .await
            .map_err(|_| "downstream_backpressure")?
            .map_err(|_| "downstream_disconnected")?;
        if let Err(error) = prepare_proxy_delivery_with_retry(
            &input.state.db,
            input.request_id,
            input.tenant_id,
            input.reservation,
            input.input_token_ceiling,
            input.output_token_ceiling,
            input.requested_service_tier,
        )
        .await
        {
            log_delivery_state_failure(input.request_id, "delivery_prepare", &error);
            return Err("delivery_state");
        }
        if let Err(error) = confirm_proxy_delivery_with_retry(
            &input.state.db,
            input.request_id,
            input.tenant_id,
            input.reservation,
        )
        .await
        {
            log_delivery_state_failure(input.request_id, "delivery_confirm", &error);
            return Err("delivery_state");
        }
        *input.confirmed = true;
        permit.send(Ok(bytes));
    } else {
        tokio::time::timeout(MAX_DOWNSTREAM_SEND_WAIT, input.sender.send(Ok(bytes)))
            .await
            .map_err(|_| "downstream_backpressure")?
            .map_err(|_| "downstream_disconnected")?;
    }
    if billable && let Some(probe) = input.probe {
        probe.delivered_validated_output().await;
    }
    Ok(billable)
}

fn log_delivery_state_failure(request_id: Uuid, stage: &'static str, error: &AppError) {
    tracing::error!(
        %request_id,
        stage,
        error_class = delivery_error_class(error),
        "proxy delivery state transition failed"
    );
}

fn delivery_error_class(error: &AppError) -> &'static str {
    match error {
        AppError::Internal => "internal",
        AppError::Storage(_) => "storage",
        AppError::Conflict(_) => "state_conflict",
        AppError::NotFound => "owner_missing",
        AppError::BadRequest(_) => "invalid_state",
        AppError::Overloaded => "overloaded",
        AppError::Unauthorized | AppError::Forbidden => "authorization",
        AppError::UnpricedModel
        | AppError::QuotaExceeded
        | AppError::RateLimited
        | AppError::LimitExceeded { .. }
        | AppError::Upstream(_) => "unexpected",
    }
}

#[cfg(test)]
mod delivery_log_tests {
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
    };

    use super::*;

    #[derive(Clone, Default)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    struct LogWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for LogCapture {
        type Writer = LogWriter;

        fn make_writer(&'writer self) -> Self::Writer {
            LogWriter(self.0.clone())
        }
    }

    #[test]
    fn delivery_failure_log_uses_only_fixed_safe_fields() {
        const CANARY: &str = "DELIVERY_DATABASE_SECRET_CANARY";
        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(capture.clone())
            .finish();
        let request_id = Uuid::now_v7();

        tracing::subscriber::with_default(subscriber, || {
            log_delivery_state_failure(
                request_id,
                "delivery_prepare",
                &AppError::Conflict(CANARY.to_owned()),
            );
            log_delivery_state_failure(request_id, "delivery_confirm", &AppError::Internal);
        });

        let rendered = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(rendered.contains(&request_id.to_string()));
        assert!(rendered.contains("delivery_prepare"));
        assert!(rendered.contains("delivery_confirm"));
        assert!(rendered.contains("state_conflict"));
        assert!(rendered.contains("internal"));
        assert!(!rendered.contains(CANARY));
    }
}

/// Keep only the bounded terminal tail, never the streamed response body.
#[derive(Default)]
pub(super) struct TerminalFrames {
    frames: Vec<SseDeliveryFrame>,
    bytes: usize,
}

impl TerminalFrames {
    pub(super) fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
    pub(super) fn hold(&mut self, frame: SseDeliveryFrame) -> Result<Option<SseDeliveryFrame>, ()> {
        if !frame.terminal && self.frames.is_empty() {
            return Ok(Some(frame));
        }
        self.bytes = self.bytes.checked_add(frame.bytes.len()).ok_or(())?;
        if self.bytes > crate::api::limits::MAX_SSE_FRAMED_BYTES_PER_NETWORK_CHUNK {
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

pub(super) fn invalid_terminal_failure(protocol: Protocol) -> Bytes {
    match protocol {
        Protocol::OpenAiResponses => crate::api::sse::safe_failure_event(),
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
