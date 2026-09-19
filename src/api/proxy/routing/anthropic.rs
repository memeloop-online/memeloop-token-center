use super::*;
use crate::api::{
    proxy::upstream_response::{UPSTREAM_STREAM_ERROR, UpstreamByteStream},
    responses_via_anthropic,
    sse::{BoundedSseFramer, parse_sse_event},
};
use std::collections::VecDeque;

pub(super) fn prepare_forwarded_request(
    route: &ResolvedUpstream,
    request: &Value,
    normalize_multi_agent: bool,
) -> Result<(Value, responses_via_anthropic::Context), AppError> {
    let mut forwarded = request.clone();
    if normalize_multi_agent {
        crate::api::request_normalization::normalize_codex_multi_agent_v2(&mut forwarded, true)?;
    }
    let context = responses_via_anthropic::prepare(&route.upstream_model, &mut forwarded)?;
    Ok((forwarded, context))
}

struct StreamState {
    upstream: UpstreamByteStream,
    framer: BoundedSseFramer,
    translator: responses_via_anthropic::Stream,
    pending: VecDeque<Result<Bytes, &'static str>>,
    failed: bool,
    diagnostic: crate::api::proxy_diagnostics::Context,
}

impl StreamState {
    fn observe(&mut self, chunk: &[u8]) -> Result<(), &'static str> {
        let framed = self.framer.push(chunk);
        if let Some(rejection) = framed.rejection {
            return Err(rejection.error_code());
        }
        for frame in framed.events {
            let (name, data) = parse_sse_event(&frame)?;
            let Some(data) = data else {
                continue;
            };
            let value =
                crate::api::sse::parse_unique_json(&data).map_err(|_| "anthropic_json_invalid")?;
            let events = self.translator.observe(name.as_deref(), &value)?;
            self.pending
                .extend(events.into_iter().map(|event| Ok(Bytes::from(event))));
        }
        Ok(())
    }

    fn report_failure(&self, stage: &'static str, reason: &'static str) {
        tracing::warn!(
            phase = "responses_anthropic_translation",
            request_id = %self.diagnostic.request_id,
            request_elapsed_ms = self.diagnostic.elapsed_millis_at(std::time::Instant::now()),
            stage,
            error_kind = reason,
            "Responses-via-Anthropic translation failed"
        );
    }

    fn into_stream(self) -> UpstreamByteStream {
        Box::pin(futures_util::stream::unfold(self, |mut state| async move {
            loop {
                if let Some(output) = state.pending.pop_front() {
                    return Some((output, state));
                }
                if state.failed {
                    return None;
                }
                match state.upstream.next().await {
                    Some(Ok(chunk)) => {
                        if let Err(reason) = state.observe(&chunk) {
                            state.report_failure("observe", reason);
                            state.pending.clear();
                            state.failed = true;
                            return Some((Err(UPSTREAM_STREAM_ERROR), state));
                        }
                    }
                    Some(Err(error)) => {
                        state.report_failure("body_read", error);
                        state.failed = true;
                        return Some((Err(error), state));
                    }
                    None => {
                        state.failed = true;
                        if !state.framer.is_complete() {
                            state.report_failure("eof", "sse_frame_incomplete");
                            return Some((Err(UPSTREAM_STREAM_ERROR), state));
                        }
                        if let Err(reason) = state.translator.finish() {
                            state.report_failure("eof", reason);
                            return Some((Err(UPSTREAM_STREAM_ERROR), state));
                        }
                        return None;
                    }
                }
            }
        }))
    }
}

fn transformed_body(
    mut upstream: UpstreamByteStream,
    context: responses_via_anthropic::Context,
    success: bool,
    diagnostic: crate::api::proxy_diagnostics::Context,
) -> UpstreamByteStream {
    Box::pin(futures_util::stream::once(async move {
        let result: Result<Bytes, &'static str> = async {
            let mut body = Vec::new();
            while let Some(chunk) = upstream.next().await {
                let chunk = chunk.map_err(|_| "transport_body_error")?;
                if body.len().saturating_add(chunk.len()) > MAX_PROXY_RESPONSE_BODY {
                    return Err("buffered_body_limit");
                }
                body.extend_from_slice(&chunk);
            }
            if !crate::gateway_body::memory::bounded_json_fits(&body, MAX_PROXY_RESPONSE_BODY * 3) {
                return Err("buffered_json_memory_limit");
            }
            let value =
                crate::api::sse::parse_unique_json(&body).map_err(|_| "anthropic_json_invalid")?;
            let translated = if success {
                responses_via_anthropic::buffered(&context, &value)?
            } else {
                responses_via_anthropic::error_body(&value)?
            };
            serde_json::to_vec(&translated)
                .map(Bytes::from)
                .map_err(|_| "event_serialization")
        }
        .await;
        result.map_err(|reason| {
            tracing::warn!(
                phase = "responses_anthropic_translation",
                request_id = %diagnostic.request_id,
                request_elapsed_ms = diagnostic.elapsed_millis_at(std::time::Instant::now()),
                stage = if success { "buffered" } else { "error" },
                error_kind = reason,
                "Responses-via-Anthropic translation failed"
            );
            UPSTREAM_STREAM_ERROR
        })
    }))
}

pub(in crate::api::proxy) fn translate(
    response: reqwest::Response,
    context: responses_via_anthropic::Context,
    streaming: bool,
    sse_framing_limits: crate::provider::SseFramingLimits,
) -> Result<UpstreamResponse, ProxySendError> {
    let diagnostic = crate::api::proxy_diagnostics::Context::current();
    let mut parts = UpstreamResponse::from(response).into_parts();
    if parts
        .headers
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .any(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
    {
        return Err(ProxySendError::AmbiguousResponse(
            "upstream_invalid_content_encoding",
        ));
    }
    let success = parts.status.is_success();
    let media_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .unwrap_or("")
        .trim();
    if success && streaming && !media_type.eq_ignore_ascii_case("text/event-stream") {
        return Err(ProxySendError::AmbiguousResponse(
            "upstream_invalid_response",
        ));
    }
    if (!streaming || !success)
        && !media_type.eq_ignore_ascii_case("application/json")
        && !media_type.ends_with("+json")
    {
        return Err(ProxySendError::AmbiguousResponse(
            "upstream_invalid_response",
        ));
    }
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::CONTENT_ENCODING);
    parts.content_length = None;
    let translated = if success && streaming {
        StreamState {
            upstream: parts.stream,
            framer: BoundedSseFramer::with_limits(sse_framing_limits),
            translator: responses_via_anthropic::Stream::new(context),
            pending: VecDeque::new(),
            failed: false,
            diagnostic,
        }
        .into_stream()
    } else {
        transformed_body(parts.stream, context, success, diagnostic)
    };
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(if success && streaming {
            "text/event-stream"
        } else {
            "application/json"
        }),
    );
    Ok(UpstreamResponse::Prefetched {
        status: parts.status,
        headers: parts.headers,
        version: parts.version,
        content_length: None,
        stream: translated,
    })
}
