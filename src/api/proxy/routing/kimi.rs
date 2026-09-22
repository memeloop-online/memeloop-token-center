use super::super::chat_sse_usage::ChatSseUsageState;
use super::*;
use crate::api::{
    proxy::upstream_response::{UPSTREAM_STREAM_ERROR, UpstreamByteStream},
    responses_via_chat,
    sse::{BoundedSseFramer, parse_sse_event},
};
use std::collections::VecDeque;

pub(super) fn prepare_forwarded_request(
    route: &ResolvedUpstream,
    protocol: Protocol,
    request: &Value,
    prepare_plaintext_collaboration: bool,
    normalize_multi_agent: bool,
    responses_via_chat_dialect: Option<crate::provider::ResponsesViaChatDialect>,
) -> Result<(Value, Option<responses_via_chat::Context>), AppError> {
    let mut forwarded = request.clone();
    if prepare_plaintext_collaboration {
        crate::api::request_normalization::prepare_plaintext_collaboration_tools(
            &mut forwarded,
            true,
        )?;
    }
    if normalize_multi_agent {
        crate::api::request_normalization::normalize_codex_multi_agent_v2(&mut forwarded, true)?;
    }
    let is_kimi_route = route.driver == crate::oauth::managed::kimi::PROVIDER_DRIVER;
    if is_kimi_route {
        if route.base_url != crate::oauth::managed::kimi::BASE_URL {
            return Err(AppError::BadRequest(
                "Kimi OAuth requires its fixed base URL".into(),
            ));
        }
        crate::oauth::managed::kimi::validate_credential(&route.credential)?;
    }
    if matches!(protocol, Protocol::OpenAiResponses)
        && let Some(dialect) = responses_via_chat_dialect
    {
        // Operator opt-in on the upstream account: compaction requests
        // become direct summarization requests the Chat upstream can serve.
        let translate_compaction = route
            .config
            .get(crate::provider::RESPONSES_VIA_CHAT_COMPACTION_CONFIG)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let context = crate::api::responses_via_chat::prepare_with_dialect(
            &route.upstream_model,
            &mut forwarded,
            dialect,
            translate_compaction,
        )?;
        crate::api::kimi_transport::repair_responses_messages(dialect, &mut forwarded);
        return Ok((forwarded, Some(context)));
    }
    if is_kimi_route {
        crate::api::kimi_transport::prepare(protocol, &route.upstream_model, &mut forwarded)?;
        return Ok((forwarded, None));
    }
    if let Some(model) = forwarded.get_mut("model") {
        *model = Value::String(route.upstream_model.clone());
    }
    Ok((forwarded, None))
}

struct StreamState {
    upstream: UpstreamByteStream,
    framer: BoundedSseFramer,
    usage: ChatSseUsageState,
    translator: responses_via_chat::Stream,
    pending: VecDeque<Result<Bytes, &'static str>>,
    terminal: bool,
    failed: bool,
    diagnostic: crate::api::proxy_diagnostics::Context,
    event_class: &'static str,
    usage_observed: bool,
    done_observed: bool,
}

impl StreamState {
    fn report_failure(&self, stage: &'static str, reason: &'static str) {
        report_failure(
            self.diagnostic,
            stage,
            reason,
            self.event_class,
            self.usage_observed,
            self.done_observed,
        );
    }

    fn observe(&mut self, chunk: &[u8]) -> Result<(), &'static str> {
        self.event_class = "sse";
        let framed = self.framer.push(chunk);
        if let Some(rejection) = framed.rejection {
            return Err(rejection.error_code());
        }
        for frame in framed.events {
            self.event_class = "sse";
            let (_, data) = parse_sse_event(&frame).map_err(|_| "sse_field_invalid")?;
            let Some(data) = data else {
                continue;
            };
            if self.terminal {
                return Err("data_after_done");
            }
            if data == b"[DONE]" {
                self.event_class = "done";
                self.done_observed = true;
                self.usage.observe_done();
                if self.usage.usage_invalid() {
                    return Err(self
                        .usage
                        .invalid_reason()
                        .unwrap_or("chat_terminal_usage_invalid"));
                }
                self.terminal = true;
                // Hold completed until EOF. An error after [DONE] must not be
                // laundered into a completed Responses generation.
            } else {
                self.event_class = "json";
                self.usage.observe_data(&data);
                let value =
                    crate::api::sse::parse_unique_json(&data).map_err(|_| "json_invalid")?;
                self.usage_observed |= !value["usage"].is_null();
                self.event_class = if !value["error"].is_null() {
                    "provider_error"
                } else if value["choices"].as_array().is_some_and(Vec::is_empty) {
                    "usage_or_control"
                } else {
                    "choice"
                };
                if !self.usage.valid_so_far() {
                    return Err(self.usage.invalid_reason().unwrap_or("chat_chunk_schema"));
                }
                let events = self.translator.observe(&value)?;
                self.pending
                    .extend(events.into_iter().map(|event| Ok(Bytes::from(event))));
            }
        }
        Ok(())
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
                        state.report_failure("body_read", "transport_body_error");
                        state.failed = true;
                        return Some((Err(error), state));
                    }
                    None => {
                        state.failed = true;
                        // A clean EOF may replace Kimi's optional DONE marker,
                        // never its validated finish, usage, or complete framing.
                        if !state.framer.is_complete() || !state.usage.terminal_ready() {
                            state.report_failure(
                                "eof",
                                if !state.framer.is_complete() {
                                    "sse_frame_incomplete"
                                } else {
                                    state
                                        .usage
                                        .invalid_reason()
                                        .unwrap_or("terminal_evidence_missing")
                                },
                            );
                            return Some((Err(UPSTREAM_STREAM_ERROR), state));
                        }
                        match state.translator.finish() {
                            Ok(events) => state
                                .pending
                                .extend(events.into_iter().map(|event| Ok(Bytes::from(event)))),
                            Err(reason) => {
                                state.report_failure("eof", reason);
                                return Some((Err(UPSTREAM_STREAM_ERROR), state));
                            }
                        }
                    }
                }
            }
        }))
    }
}

fn report_failure(
    context: crate::api::proxy_diagnostics::Context,
    stage: &'static str,
    reason: &'static str,
    event_class: &'static str,
    usage_observed: bool,
    done_observed: bool,
) {
    // All labels are code-owned. Never emit provider text, JSON, or serde errors.
    tracing::warn!(phase = "responses_chat_translation", request_id = %context.request_id,
        request_elapsed_ms = context.elapsed_millis_at(std::time::Instant::now()),
        stage, error_kind = reason, event_class, usage_observed, done_observed,
        "Responses-via-Chat translation failed");
}

pub(in crate::api::proxy) fn translate(
    response: reqwest::Response,
    context: responses_via_chat::Context,
    streaming: bool,
    sse_framing_limits: crate::provider::SseFramingLimits,
) -> Result<UpstreamResponse, ProxySendError> {
    if !response.status().is_success() {
        return Ok(response.into());
    }
    let diagnostic = crate::api::proxy_diagnostics::Context::current();
    let mut parts = UpstreamResponse::from(response).into_parts();
    if parts
        .headers
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .any(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
    {
        report_failure(
            diagnostic,
            "headers",
            "content_encoding_invalid",
            "none",
            false,
            false,
        );
        return Err(ProxySendError::AmbiguousResponse(
            "upstream_invalid_content_encoding",
        ));
    }
    let media_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .unwrap_or("")
        .trim();
    if streaming && !media_type.eq_ignore_ascii_case("text/event-stream") {
        report_failure(
            diagnostic,
            "headers",
            "content_type_invalid",
            "none",
            false,
            false,
        );
        return Err(ProxySendError::AmbiguousResponse(
            "upstream_invalid_response",
        ));
    }
    if !streaming && !media_type.eq_ignore_ascii_case("application/json") {
        report_failure(
            diagnostic,
            "headers",
            "content_type_invalid",
            "none",
            false,
            false,
        );
        return Err(ProxySendError::AmbiguousResponse(
            "upstream_invalid_response",
        ));
    }
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::CONTENT_ENCODING);
    parts.content_length = None;
    let kimi_dialect = context.uses_kimi_dialect();
    let translated = if streaming {
        let state = StreamState {
            upstream: parts.stream,
            framer: BoundedSseFramer::with_limits(sse_framing_limits),
            usage: if kimi_dialect {
                ChatSseUsageState::for_kimi()
            } else {
                ChatSseUsageState::default()
            },
            translator: responses_via_chat::Stream::new(context),
            pending: VecDeque::new(),
            terminal: false,
            failed: false,
            diagnostic,
            event_class: "none",
            usage_observed: false,
            done_observed: false,
        };
        state.into_stream()
    } else {
        Box::pin(futures_util::stream::once(async move {
            let mut usage_observed = false;
            let result: Result<Bytes, &'static str> = async {
                let mut upstream = parts.stream;
                let mut body = Vec::new();
                while let Some(chunk) = upstream.next().await {
                    let chunk = chunk.map_err(|_| "transport_body_error")?;
                    if body.len().saturating_add(chunk.len()) > MAX_PROXY_RESPONSE_BODY {
                        return Err("buffered_body_limit");
                    }
                    body.extend_from_slice(&chunk);
                }
                body.shrink_to_fit();
                if !crate::gateway_body::memory::bounded_json_fits(
                    &body,
                    MAX_PROXY_RESPONSE_BODY * 3,
                ) {
                    return Err("buffered_json_memory_limit");
                }
                let value =
                    crate::api::sse::parse_unique_json(&body).map_err(|_| "json_invalid")?;
                usage_observed = !value["usage"].is_null();
                drop(body);
                let response = responses_via_chat::buffered(&context, &value)?;
                drop(value);
                serde_json::to_vec(&response)
                    .map(Bytes::from)
                    .map_err(|_| "event_serialization")
            }
            .await;
            result.map_err(|reason| {
                report_failure(
                    diagnostic,
                    "buffered",
                    reason,
                    "buffered",
                    usage_observed,
                    false,
                );
                UPSTREAM_STREAM_ERROR
            })
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
#[path = "kimi_diagnostics_tests.rs"]
mod diagnostics_tests;

#[cfg(test)]
#[path = "kimi_terminal_tests.rs"]
mod terminal_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    fn route(driver: &str) -> ResolvedUpstream {
        ResolvedUpstream {
            route_id: uuid::Uuid::nil(),
            account_id: uuid::Uuid::nil(),
            transport_revision: 1,
            credential_generation: 1,
            driver: driver.to_owned(),
            base_url: "https://upstream.invalid".to_owned(),
            config: json!({}),
            upstream_model: "upstream-model".to_owned(),
            credential: crate::provider::UpstreamCredential::None,
        }
    }

    fn kimi_route() -> ResolvedUpstream {
        ResolvedUpstream {
            route_id: uuid::Uuid::nil(),
            account_id: uuid::Uuid::nil(),
            transport_revision: 1,
            credential_generation: 1,
            driver: crate::oauth::managed::kimi::PROVIDER_DRIVER.to_owned(),
            base_url: crate::oauth::managed::kimi::BASE_URL.to_owned(),
            config: json!({}),
            upstream_model: "kimi-k3-256k".to_owned(),
            credential: crate::provider::UpstreamCredential::OAuth {
                access_token: "access-token".to_owned(),
                refresh_token: Some("refresh-token".to_owned()),
                expires_at: None,
                header: "authorization".to_owned(),
                prefix: "Bearer ".to_owned(),
                adapter_state: Some(json!({
                    "schema": "kimi-oauth-v1",
                    "device_id": null,
                    "scope": null,
                    "token_type": "bearer"
                })),
                proxy_url: None,
                proxy_network_scope: None,
            },
        }
    }

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
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
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

    #[test]
    fn compaction_translation_is_an_upstream_config_opt_in() {
        let request = json!({"model":"public-model","stream":true,
        "tools":[{"type":"function","name":"exec","parameters":{"type":"object"}}],
        "input":[
            {"role":"user","content":"earlier work"},
            {"type":"compaction_trigger"}
        ]});
        let dialect = Some(crate::provider::ResponsesViaChatDialect::KimiV1);

        // A driver with a declared Responses-via-Chat dialect, distinct from
        // the managed Kimi OAuth driver (whose fixed base URL and credential
        // shape this test does not need).
        let dialect_driver = "responses-chat-custom";
        let mut opted = route(dialect_driver);
        opted.config = json!({"responses_via_chat_compaction": true});
        let (forwarded, _) = prepare_forwarded_request(
            &opted,
            Protocol::OpenAiResponses,
            &request,
            false,
            false,
            dialect,
        )
        .unwrap();
        let messages = forwarded["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "earlier work");
        assert_eq!(messages[1]["role"], "user");
        assert!(
            messages[1]["content"]
                .as_str()
                .unwrap()
                .contains("CONTEXT CHECKPOINT COMPACTION")
        );
        assert_eq!(messages.len(), 2);
        assert!(forwarded.get("tools").is_none());

        let plain = route(dialect_driver);
        assert!(
            prepare_forwarded_request(
                &plain,
                Protocol::OpenAiResponses,
                &request,
                false,
                false,
                dialect,
            )
            .is_err()
        );
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
            responses_via_chat::Context::for_kimi(&json!({"model":"kimi-k3"})),
            true,
            crate::provider::SseFramingLimits::default(),
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
                responses_via_chat::Context::for_kimi(&json!({"model":"kimi-k3"})),
                true,
                crate::provider::SseFramingLimits::default(),
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

    #[test]
    fn native_parent_route_marks_collaboration_messages_plaintext_only() {
        let request = json!({
            "model": "public-model",
            "tools": [{"type":"namespace","name":"collaboration","tools":[{
                "type":"function","name":"spawn_agent","parameters":{
                    "type":"object","properties":{"message":{"type":"string","encrypted":{"type":"boolean"}}}
                }
            }]}],
            "input": [{"type":"additional_tools","tools":[{"type":"namespace","name":"collaboration","tools":[{
                "type":"function","name":"followup_task","parameters":{
                    "type":"object","properties":{"message":{"type":"string","encrypted":{"type":"boolean"}}}
                }
            }]}]}]
        });
        let (forwarded, _) = prepare_forwarded_request(
            &route("openai-codex"),
            Protocol::OpenAiResponses,
            &request,
            true,
            false,
            None,
        )
        .unwrap();

        assert!(
            forwarded["tools"][0]["tools"][0]["parameters"]["properties"]["message"]
                .get("encrypted")
                .is_none()
        );
        assert!(
            forwarded["input"][0]["tools"][0]["tools"][0]["parameters"]["properties"]["message"]
                .get("encrypted")
                .is_none()
        );
        assert_eq!(forwarded["tools"][0]["name"], "collaboration");
        assert_eq!(forwarded["input"][0]["type"], "additional_tools");
    }

    #[test]
    fn openai_compatible_http_keeps_responses_and_rewrites_readable_agent_message() {
        let request = json!({
            "model": "public-model",
            "tools": [{"type":"function","name":"spawn_agent","parameters":{
                "type":"object","properties":{"message":{"type":"string","encrypted":true}}
            }}],
            "input": [
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]},
                {"type":"agent_message","role":"system","content":[
                    {"type":"input_text","text":"delegated task"}
                ]}
            ]
        });
        let (forwarded, chat) = prepare_forwarded_request(
            &route("http-json"),
            Protocol::OpenAiResponses,
            &request,
            true,
            true,
            None,
        )
        .unwrap();

        assert!(chat.is_none());
        assert!(forwarded.get("messages").is_none());
        assert_eq!(forwarded["model"], "upstream-model");
        assert!(
            forwarded["tools"][0]["parameters"]["properties"]["message"]
                .get("encrypted")
                .is_none()
        );
        assert_eq!(forwarded["input"][1]["type"], "message");
        assert_eq!(forwarded["input"][1]["role"], "user");
    }

    #[test]
    fn direct_and_resume_requests_bypass_collaboration_schema_rewrite() {
        for request in [
            json!({"model":"public-model","input":"direct request"}),
            json!({
                "model":"public-model",
                "previous_response_id":"response-for-resume",
                "input":[{"type":"message","role":"user","content":[
                    {"type":"input_text","text":"resume request"}
                ]}]
            }),
        ] {
            let (forwarded, _) = prepare_forwarded_request(
                &route("openai-codex"),
                Protocol::OpenAiResponses,
                &request,
                false,
                false,
                None,
            )
            .unwrap();

            assert_eq!(forwarded["input"], request["input"]);
            assert_eq!(
                forwarded.get("previous_response_id"),
                request.get("previous_response_id")
            );
        }
    }

    #[test]
    fn third_party_route_downgrades_only_readable_agent_message() {
        let request = json!({
            "model": "public-model",
            "input": [{"type":"agent_message","role":"system",
                "author":"internal",
                "content":[
                    {"type":"input_text","text":"delegated task"},
                    {"type":"encrypted_content","encrypted_content":"opaque-ciphertext"}
                ]
            }]
        });
        let (forwarded, _) = prepare_forwarded_request(
            &kimi_route(),
            Protocol::OpenAiResponses,
            &request,
            true,
            true,
            Some(crate::provider::ResponsesViaChatDialect::KimiV1),
        )
        .unwrap();

        assert_eq!(forwarded["messages"][0]["role"], "user");
        assert!(forwarded["messages"][0].get("author").is_none());
        assert_eq!(forwarded["messages"][0]["content"][0]["type"], "text");
        assert_eq!(
            forwarded["messages"][0]["content"][0]["text"],
            "delegated task"
        );
        assert!(!forwarded.to_string().contains("opaque-ciphertext"));
    }

    #[test]
    fn third_party_route_rejects_opaque_agent_message_before_dispatch() {
        let request = json!({
            "model": "public-model",
            "input": [{"type":"agent_message","content":[
                {"type":"encrypted_content","encrypted_content":"opaque-ciphertext"}
            ]}]
        });
        let error = match prepare_forwarded_request(
            &kimi_route(),
            Protocol::OpenAiResponses,
            &request,
            true,
            true,
            Some(crate::provider::ResponsesViaChatDialect::KimiV1),
        ) {
            Ok(_) => panic!("opaque delegated content must fail closed before dispatch"),
            Err(error) => error,
        };
        assert!(
            matches!(error, AppError::BadRequest(message) if message.contains("agent_message"))
        );
    }
}
