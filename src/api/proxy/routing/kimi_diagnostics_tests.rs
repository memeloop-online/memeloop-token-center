use super::*;
use std::sync::{Arc, Mutex};
use tracing::instrument::WithSubscriber;

fn state() -> StreamState {
    StreamState {
        upstream: Box::pin(futures_util::stream::empty()),
        framer: BoundedSseFramer::default(),
        usage: ChatSseUsageState::default(),
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
fn failed_done_preserves_first_contract_reason_and_observed_usage() {
    let mut state = state();
    state
        .observe(&data(
            json!({"id":"test", "object":"chat.completion.chunk", "model":"k3",
        "choices":[{"index":0,"delta":{"content":"canary-content"},"finish_reason":null}],
        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}),
        ))
        .unwrap();
    assert_eq!(
        state.observe(b"data: [DONE]\n\n"),
        Err("chat_usage_on_choice_or_choice_after_usage")
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
        Err("choices_missing")
    );
    assert_eq!(state.translator.finish(), Err("finish_reason_missing"));
    let mut translator =
        responses::Stream::new(responses::Context::new(&json!({"model":"kimi-k3"})));
    translator
        .observe(&json!({"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}))
        .unwrap();
    assert_eq!(translator.finish(), Err("finish_reason_length"));
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
            let reason = state
                .observe(b"data: {\"choices\":\"canary-secret\"}\n\n")
                .unwrap_err();
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
    assert_eq!(value["fields"]["error_kind"], "choices_missing");
    assert_eq!(value["fields"]["phase"], "kimi_response_translation");
    assert!(value["fields"]["request_elapsed_ms"].as_i64().unwrap() >= 60_000);
}
