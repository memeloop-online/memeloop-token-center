use super::*;
use std::sync::{Arc, Mutex};
use tracing::instrument::WithSubscriber;

fn state() -> StreamState {
    StreamState {
        upstream: Box::pin(futures_util::stream::empty()),
        framer: BoundedSseFramer::default(),
        usage: ChatSseUsageState::for_kimi(),
        translator: responses::Stream::new(responses::Context::new(&json!({"model":"kimi-k3"}))),
        pending: VecDeque::new(),
        terminal: false,
        failed: false,
        diagnostic: crate::api::proxy_diagnostics::Context::current(),
        event_class: "none",
        usage_observed: false,
        done_observed: false,
    }
}

fn data(value: Value) -> Vec<u8> {
    format!("data: {value}\n\n").into_bytes()
}

#[test]
fn invalid_usage_fails_on_its_frame_without_waiting_for_done() {
    let mut state = state();
    let result = state.observe(&data(
        json!({"id":"test", "object":"chat.completion.chunk", "model":"k3",
        "choices":[{"index":0,"delta":{"content":"canary-content"},"finish_reason":null}],
        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}),
    ));
    assert_eq!(result, Err("chat_usage_sequence"));
    assert!(state.usage_observed);
    assert!(!state.done_observed);
    assert_eq!(state.event_class, "choice");
    assert_eq!(
        state.observe(b"data: [DONE]\n\n"),
        Err("chat_usage_sequence")
    );
    assert!(state.usage_observed);
    assert!(state.done_observed);
    assert!(!state.terminal);
    assert_eq!(state.event_class, "done");
}

#[test]
fn translation_reasons_distinguish_usage_finish_and_limits_without_payload() {
    let mut state = state();
    assert_eq!(
        state.observe(b"data: {\"choices\":\"canary-secret\"}\n\n"),
        Err("kimi_choices_type")
    );
    // Schema rejection now happens before any translator output is created.
    assert!(state.pending.is_empty());
    assert_eq!(state.translator.finish(), Err("empty_stream"));
    let mut translator =
        responses::Stream::new(responses::Context::new(&json!({"model":"kimi-k3"})));
    translator
        .observe(&json!({"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}))
        .unwrap();
    translator.observe(&json!({"choices":[],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}})).unwrap();
    let incomplete = String::from_utf8(translator.finish().unwrap().concat()).unwrap();
    assert!(incomplete.contains("response.incomplete"));
    assert!(!incomplete.contains("response.completed"));
    let mut translator =
        responses::Stream::new(responses::Context::new(&json!({"model":"kimi-k3"})));
    translator
        .observe(&json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}))
        .unwrap();
    assert_eq!(translator.finish(), Err("usage_missing"));
    assert_eq!(translator.observe(&json!({"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":4}})), Err("usage_total_mismatch"));
    let calls = (0..513)
        .map(|index| json!({"index":index,"id":"call","function":{"name":"tool","arguments":""}}))
        .collect::<Vec<_>>();
    assert_eq!(
        translator.observe(&json!({"choices":[{"index":0,"delta":{"tool_calls":calls}}]})),
        Err("item_limit")
    );
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn schema_failure_logs_the_first_frame_category_without_provider_field_names() {
    let capture = Capture::default();
    let _other_dispatch = tracing::Dispatch::new(tracing_subscriber::registry());
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_writer(capture.clone())
            .finish(),
    );
    tracing::dispatcher::with_default(&dispatch, || {
        let mut state = state();
        let reason = state
            .observe(&data(json!({
                "id":"test", "object":"chat.completion.chunk", "model":"k3",
                "choices":[{"index":0,"delta":{"content":"canary-secret"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2,"canary-secret":1}
            })))
            .unwrap_err();
        assert_eq!(reason, "kimi_usage_unknown_field");
        assert!(!state.done_observed);
        state.report_failure("observe", reason);
    });
    let logged = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(!logged.contains("canary-secret"));
    let value: Value = serde_json::from_str(logged.trim()).unwrap();
    assert_eq!(value["fields"]["error_kind"], "kimi_usage_unknown_field");
    assert_eq!(value["fields"]["event_class"], "choice");
    assert_eq!(value["fields"]["done_observed"], false);
}

#[tokio::test]
async fn owned_failure_keeps_ingress_identity_clock_and_never_logs_payload() {
    let capture = Capture::default();
    // Keep distinct live dispatchers, avoiding globally cached disabled callsites
    // when other tests execute without this capture subscriber in parallel.
    let _other_dispatch = tracing::Dispatch::new(tracing_subscriber::registry());
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_writer(capture.clone())
            .finish(),
    );
    let id = Uuid::now_v7();
    let context = crate::api::proxy_diagnostics::Context::with_started_for_test(
        id,
        std::time::Instant::now() - std::time::Duration::from_secs(60),
    );
    let mut state = crate::api::proxy_diagnostics::CONTEXT
        .scope(context, async { state() })
        .await;
    tokio::spawn(
        async move {
            state
                .observe(&data(
                    json!({"id":"test", "object":"chat.completion.chunk", "model":"k3",
                "choices":[{"index":0,"delta":{"content":"canary-secret"},"finish_reason":null}]}),
                ))
                .unwrap();
            assert_eq!(state.event_class, "choice");
            let mut oversized = b"data: canary-secret ".to_vec();
            oversized.resize(crate::api::limits::MAX_RESPONSES_SSE_EVENT_BYTES + 1, b'x');
            let reason = state.observe(&oversized).unwrap_err();
            assert_eq!(state.event_class, "sse");
            state.report_failure("observe", reason);
        }
        .with_subscriber(dispatch),
    )
    .await
    .unwrap();
    let bytes = capture.0.lock().unwrap().clone();
    let logged = String::from_utf8(bytes).unwrap();
    assert!(!logged.contains("canary-secret"));
    let value: Value = serde_json::from_str(logged.trim()).unwrap();
    assert_eq!(value["fields"]["request_id"], id.to_string());
    assert_eq!(
        value["fields"]["error_kind"],
        "upstream_response_event_too_large"
    );
    assert_eq!(value["fields"]["event_class"], "sse");
    assert_eq!(value["fields"]["phase"], "responses_chat_translation");
    assert!(value["fields"]["request_elapsed_ms"].as_i64().unwrap() >= 60_000);
}
