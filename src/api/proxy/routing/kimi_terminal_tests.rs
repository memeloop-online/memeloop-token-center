use super::*;
use crate::api::responses_via_chat;

fn wire(finish: Option<&str>, usage: Value, delta: Value) -> Bytes {
    Bytes::from(format!(
        "data: {}\n\n",
        json!({"id":"chat-test","object":"chat.completion.chunk","model":"k3",
        "choices":[{"index":0,"delta":delta,"finish_reason":finish}],"usage":usage})
    ))
}

fn usage() -> Value {
    json!({"prompt_tokens":5,"completion_tokens":2,"total_tokens":7})
}

#[tokio::test]
async fn malformed_accounting_never_completes_responses_or_validates_native_chat_usage() {
    for usage in crate::api::kimi_transport::usage::invalid_examples() {
        let chunk = wire(Some("stop"), usage, json!({"content":"fixture"}));
        let mut native = ResponsesSseCapture::for_kimi_chat_usage();
        native.push(&chunk);
        native.push(b"data: [DONE]\n\n");
        let summary = native.finish_summary();
        assert!(summary.usage_invalid);
        assert!(summary.usage.is_none());
        let output = state(vec![Ok(chunk), Ok(Bytes::from_static(b"data: [DONE]\n\n"))])
            .into_stream()
            .collect::<Vec<_>>()
            .await;
        assert!(output.iter().any(Result::is_err));
        let text = String::from_utf8(
            output
                .into_iter()
                .filter_map(Result::ok)
                .flatten()
                .collect(),
        )
        .unwrap();
        assert!(!text.contains("response.completed"));
    }
}

fn state(chunks: Vec<Result<Bytes, &'static str>>) -> StreamState {
    StreamState {
        upstream: Box::pin(futures_util::stream::iter(chunks)),
        framer: BoundedSseFramer::default(),
        usage: ChatSseUsageState::for_kimi(),
        translator: responses_via_chat::Stream::new(responses_via_chat::Context::for_kimi(
            &json!({"model":"kimi-k3","tools":[{
                "type":"function","name":"tool","parameters":{"type":"object"}
            }]}),
        )),
        pending: VecDeque::new(),
        terminal: false,
        failed: false,
        diagnostic: crate::api::proxy_diagnostics::Context::current(),
        event_class: "none",
        usage_observed: false,
        done_observed: false,
    }
}

#[tokio::test]
async fn clean_eof_terminal_choice_usage_completes_once_and_strict_chat_is_unchanged() {
    let chunk = wire(Some("stop"), usage(), json!({"content":"ok"}));
    let mut strict = ChatSseUsageState::default();
    let json = &chunk[b"data: ".len()..chunk.len() - 2];
    strict.observe_data(json);
    assert_eq!(
        strict.invalid_reason(),
        Some("chat_usage_on_choice_or_choice_after_usage")
    );
    let output = state(vec![Ok(chunk)])
        .into_stream()
        .collect::<Vec<_>>()
        .await;
    assert!(output.iter().all(Result::is_ok));
    let mut capture = ResponsesSseCapture::for_responses();
    let mut text = String::new();
    for chunk in output {
        let chunk = chunk.unwrap();
        capture.push(&chunk);
        text.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    assert_eq!(text.matches("event: response.completed\n").count(), 1);
    let summary = capture.finish_summary();
    assert!(matches!(
        summary.outcome,
        ResponsesSseOutcome::Completed { .. }
    ));
    assert!(!summary.usage_invalid);
    assert_eq!(summary.usage.unwrap().output_tokens, 2);
}

#[tokio::test]
async fn eof_without_terminal_evidence_and_transport_error_never_complete() {
    let finished = wire(Some("stop"), usage(), json!({"content":"ok"}));
    for chunks in [
        vec![],
        vec![Ok(wire(None, Value::Null, json!({"content":"prefix"})))],
        vec![Ok(wire(
            Some("stop"),
            Value::Null,
            json!({"content":"prefix"}),
        ))],
        vec![Ok(wire(None, usage(), json!({"content":"prefix"})))],
        vec![Ok(finished.clone()), Err("upstream_stream")],
        vec![
            Ok(finished.clone()),
            Ok(Bytes::from_static(b"data: {\"unterminated\"")),
        ],
        vec![
            Ok(finished.clone()),
            Ok(wire(
                Some("stop"),
                json!({"prompt_tokens":5,"completion_tokens":3,"total_tokens":8}),
                json!({}),
            )),
        ],
        vec![
            Ok(finished.clone()),
            Ok(Bytes::from_static(b"data: [DONE]\n\ndata: {}\n\n")),
        ],
    ] {
        let output = state(chunks).into_stream().collect::<Vec<_>>().await;
        assert!(output.iter().any(Result::is_err));
        let text = String::from_utf8(
            output
                .into_iter()
                .filter_map(Result::ok)
                .flatten()
                .collect(),
        )
        .unwrap();
        assert!(!text.contains("response.completed"));
    }
}

#[tokio::test]
async fn eof_completes_only_complete_tools_and_length_is_incomplete() {
    for (call, success) in [
        (
            json!({"index":0,"id":"call-1","function":{"name":"tool","arguments":"{}"}}),
            true,
        ),
        (
            json!({"index":0,"function":{"name":"tool","arguments":"{}"}}),
            false,
        ),
        (
            json!({"index":0,"id":"call-1","function":{"arguments":"{}"}}),
            false,
        ),
        (
            json!({"index":0,"id":"call-1","function":{"name":"tool","arguments":"{"}}),
            false,
        ),
    ] {
        let output = state(vec![Ok(wire(
            Some("tool_calls"),
            usage(),
            json!({"tool_calls":[call]}),
        ))])
        .into_stream()
        .collect::<Vec<_>>()
        .await;
        assert_eq!(output.iter().all(Result::is_ok), success);
        let text = String::from_utf8(
            output
                .into_iter()
                .filter_map(Result::ok)
                .flatten()
                .collect(),
        )
        .unwrap();
        assert_eq!(text.contains("event: response.completed\n"), success);
    }
    for reason in ["length", "content_filter"] {
        let output = state(vec![Ok(wire(
            Some(reason),
            usage(),
            json!({"content":"partial"}),
        ))])
        .into_stream()
        .collect::<Vec<_>>()
        .await;
        assert!(output.iter().all(Result::is_ok));
        let text =
            String::from_utf8(output.into_iter().flat_map(Result::unwrap).collect()).unwrap();
        assert!(text.contains("event: response.incomplete\n"));
        assert!(!text.contains("response.completed"));
    }
}

#[tokio::test]
async fn duplicate_call_ids_and_incomplete_custom_arguments_never_complete() {
    let duplicate = json!({"tool_calls":[
        {"index":0,"id":"same","function":{"name":"tool","arguments":"{}"}},
        {"index":1,"id":"same","function":{"name":"tool","arguments":"{}"}}
    ]});
    let custom = json!({"tool_calls":[{"index":0,"id":"custom-call","function":{"name":"patch","arguments":"{\"input\":"}}]});
    for delta in [duplicate, custom] {
        let mut state = state(vec![Ok(wire(Some("tool_calls"), usage(), delta))]);
        state.translator = responses_via_chat::Stream::new(responses_via_chat::Context::for_kimi(
            &json!({"model":"kimi-k3",
            "tools":[{"type":"custom","name":"patch"}]}),
        ));
        let output = state.into_stream().collect::<Vec<_>>().await;
        assert!(output.iter().any(Result::is_err));
        let text = String::from_utf8(
            output
                .into_iter()
                .filter_map(Result::ok)
                .flatten()
                .collect(),
        )
        .unwrap();
        assert!(!text.contains("response.completed"));
    }
}
