use super::super::chat_sse_usage::ChatSseUsageState;
use super::*;
use crate::api::{
    kimi_transport::responses,
    sse::{BoundedSseFramer, parse_sse_event},
};
use std::collections::VecDeque;

pub(super) fn prepare_forwarded_request(
    route: &ResolvedUpstream,
    protocol: Protocol,
    request: &Value,
) -> Result<(Value, Option<responses::Context>), AppError> {
    let mut forwarded = request.clone();
    if route.driver != crate::oauth::managed::kimi::PROVIDER_DRIVER {
        if let Some(model) = forwarded.get_mut("model") {
            *model = Value::String(route.upstream_model.clone());
        }
        return Ok((forwarded, None));
    }
    if route.base_url != crate::oauth::managed::kimi::BASE_URL {
        return Err(AppError::BadRequest(
            "Kimi OAuth requires its fixed base URL".into(),
        ));
    }
    crate::oauth::managed::kimi::validate_credential(&route.credential)?;
    let context =
        matches!(protocol, Protocol::OpenAiResponses).then(|| responses::Context::new(request));
    crate::api::kimi_transport::prepare(protocol, &route.upstream_model, &mut forwarded)?;
    Ok((forwarded, context))
}

struct StreamState {
    upstream: super::super::upstream_response::UpstreamByteStream,
    framer: BoundedSseFramer,
    usage: ChatSseUsageState,
    translator: responses::Stream,
    pending: VecDeque<Result<Bytes, ()>>,
    terminal: bool,
    failed: bool,
}

impl StreamState {
    fn observe(&mut self, chunk: &[u8]) -> Result<(), ()> {
        let framed = self.framer.push(chunk);
        if framed.rejection.is_some() {
            return Err(());
        }
        for frame in framed.events {
            let (_, data) = parse_sse_event(&frame).map_err(|_| ())?;
            let Some(data) = data else {
                continue;
            };
            if self.terminal {
                return Err(());
            }
            if data == b"[DONE]" {
                self.usage.observe_done();
                if self.usage.usage_invalid() {
                    return Err(());
                }
                self.terminal = true;
                // Hold completed until EOF. An error after [DONE] must not be
                // laundered into a completed Responses generation.
            } else {
                self.usage.observe_data(&data);
                let value = crate::api::sse::parse_unique_json(&data).map_err(|_| ())?;
                let events = self.translator.observe(&value)?;
                self.pending
                    .extend(events.into_iter().map(|event| Ok(Bytes::from(event))));
            }
        }
        Ok(())
    }
}

pub(super) fn translate(
    response: reqwest::Response,
    context: responses::Context,
    streaming: bool,
) -> Result<UpstreamResponse, ProxySendError> {
    if !response.status().is_success() {
        return Ok(response.into());
    }
    let mut parts = UpstreamResponse::from(response).into_parts();
    let media_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .unwrap_or("")
        .trim();
    if streaming && !media_type.eq_ignore_ascii_case("text/event-stream") {
        return Err(ProxySendError::AmbiguousResponse(
            "upstream_invalid_response",
        ));
    }
    if !streaming && !media_type.eq_ignore_ascii_case("application/json") {
        return Err(ProxySendError::AmbiguousResponse(
            "upstream_invalid_response",
        ));
    }
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::CONTENT_ENCODING);
    parts.content_length = None;
    let translated = if streaming {
        let state = StreamState {
            upstream: parts.stream,
            framer: BoundedSseFramer::default(),
            usage: ChatSseUsageState::default(),
            translator: responses::Stream::new(context),
            pending: VecDeque::new(),
            terminal: false,
            failed: false,
        };
        Box::pin(futures_util::stream::unfold(
            state,
            |mut state| async move {
                loop {
                    if let Some(output) = state.pending.pop_front() {
                        return Some((output, state));
                    }
                    if state.failed {
                        return None;
                    }
                    match state.upstream.next().await {
                        Some(Ok(chunk)) => {
                            if state.observe(&chunk).is_err() {
                                state.pending.clear();
                                state.failed = true;
                                return Some((Err(()), state));
                            }
                        }
                        Some(Err(())) => {
                            state.failed = true;
                            return Some((Err(()), state));
                        }
                        None => {
                            state.failed = true;
                            if !state.terminal || !state.framer.is_complete() {
                                return Some((Err(()), state));
                            }
                            match state.translator.finish() {
                                Ok(events) => state
                                    .pending
                                    .extend(events.into_iter().map(|event| Ok(Bytes::from(event)))),
                                Err(()) => return Some((Err(()), state)),
                            }
                        }
                    }
                }
            },
        )) as super::super::upstream_response::UpstreamByteStream
    } else {
        Box::pin(futures_util::stream::once(async move {
            let mut upstream = parts.stream;
            let mut body = Vec::new();
            while let Some(chunk) = upstream.next().await {
                let chunk = chunk?;
                if body.len().saturating_add(chunk.len()) > MAX_PROXY_RESPONSE_BODY {
                    return Err(());
                }
                body.extend_from_slice(&chunk);
            }
            let value = crate::api::sse::parse_unique_json(&body).map_err(|_| ())?;
            let response = responses::buffered(&context, &value)?;
            serde_json::to_vec(&response)
                .map(Bytes::from)
                .map_err(|_| ())
        }))
    };
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(if streaming {
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

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    fn chunk(choices: Value, usage: Value) -> String {
        format!(
            "data: {}\n\n",
            json!({"id":"chat-fixture","object":"chat.completion.chunk",
            "model":"k3","choices":choices,"usage":usage})
        )
    }

    async fn mock_response(body: String) -> (MockServer, reqwest::Response) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/coding/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .expect(1)
            .mount(&server)
            .await;
        let response = reqwest::Client::new()
            .post(format!("{}/coding/v1/chat/completions", server.uri()))
            .send()
            .await
            .unwrap();
        (server, response)
    }

    #[tokio::test]
    async fn real_mock_http_stream_translates_and_accounts_terminal_usage_once() {
        let body = [
            chunk(
                json!([{"index":0,"delta":{"content":"ok"},"finish_reason":null}]),
                Value::Null,
            ),
            chunk(
                json!([{"index":0,"delta":{},"finish_reason":"stop"}]),
                Value::Null,
            ),
            chunk(
                json!([]),
                json!({"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}),
            ),
            "data: [DONE]\n\n".into(),
        ]
        .concat();
        let (_server, response) = mock_response(body).await;
        let translated = translate(
            response,
            responses::Context::new(&json!({"model":"kimi-k3"})),
            true,
        )
        .unwrap();
        let chunks = translated.bytes_stream().collect::<Vec<_>>().await;
        assert!(chunks.iter().all(Result::is_ok));
        let mut capture = ResponsesSseCapture::for_responses();
        for chunk in chunks {
            capture.push(&chunk.unwrap());
        }
        let summary = capture.finish_summary();
        assert!(!summary.usage_invalid);
        let usage = summary.usage.unwrap();
        assert_eq!(usage.input_tokens, 5);
        assert_eq!(usage.output_tokens, 2);
    }

    #[tokio::test]
    async fn missing_usage_and_truncated_stream_never_emit_completed() {
        for suffix in ["data: [DONE]\n\n", ""] {
            let body = [
                chunk(
                    json!([{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]),
                    Value::Null,
                ),
                suffix.into(),
            ]
            .concat();
            let (_server, response) = mock_response(body).await;
            let translated = translate(
                response,
                responses::Context::new(&json!({"model":"kimi-k3"})),
                true,
            )
            .unwrap();
            let chunks = translated.bytes_stream().collect::<Vec<_>>().await;
            assert!(chunks.iter().any(Result::is_err));
            let output = chunks
                .into_iter()
                .filter_map(Result::ok)
                .flatten()
                .collect::<Vec<_>>();
            assert!(
                !String::from_utf8(output)
                    .unwrap()
                    .contains("response.completed")
            );
        }
    }
}
