use super::*;

fn terminal() -> Value {
    serde_json::json!({"type":"response.incomplete","response":{
        "id":"resp-incomplete","status":"incomplete","error":null,
        "incomplete_details":{"reason":"future_provider_reason"},
        "usage":{"input_tokens":10,"output_tokens":3,"total_tokens":13,
            "input_tokens_details":{"cached_tokens":4}}
    }})
}

fn capture(value: &Value, suffix: &str) -> ResponsesSseSummary {
    let mut capture = ResponsesSseCapture::for_codex_responses();
    capture.push(
        b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-incomplete\"}}\n\n",
    );
    capture.push(format!("event: response.incomplete\ndata: {value}\n\n{suffix}").as_bytes());
    capture.finish_summary()
}

#[test]
fn provider_incomplete_is_distinct_from_missing_terminal_and_keeps_exact_usage() {
    let summary = capture(&terminal(), "data: [DONE]\n\n");
    assert_eq!(summary.outcome, ResponsesSseOutcome::TerminatedIncomplete);
    assert!(!summary.protocol_invalid && !summary.usage_invalid);
    let usage = summary.usage.unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.output_tokens
        ),
        (6, 4, 3)
    );
    let mut unfinished = ResponsesSseCapture::for_codex_responses();
    unfinished.push(
        b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-incomplete\"}}\n\n",
    );
    assert_eq!(
        unfinished.finish_summary().outcome,
        ResponsesSseOutcome::Incomplete
    );
}

#[test]
fn incomplete_usage_requires_consistent_counters_identity_and_one_terminal() {
    for (pointer, bad) in [
        ("/response/usage/output_tokens", Value::Null),
        ("/response/usage/total_tokens", serde_json::json!(99)),
        (
            "/response/usage/input_tokens_details/cached_tokens",
            serde_json::json!(11),
        ),
        ("/response/id", serde_json::json!("other")),
        ("/response/status", serde_json::json!("completed")),
        ("/response/error", serde_json::json!({"message":"failed"})),
    ] {
        let mut value = terminal();
        *value.pointer_mut(pointer).unwrap() = bad;
        let summary = capture(&value, "");
        assert!(
            summary.usage_invalid
                || summary.protocol_invalid
                || summary.outcome == ResponsesSseOutcome::Failed,
            "{pointer}"
        );
    }
    let terminal = terminal();
    let duplicate = capture(&terminal, &format!("data: {terminal}\n\n"));
    assert!(duplicate.protocol_invalid);
    assert!(
        capture(
            &terminal,
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"late\"}\n\n"
        )
        .protocol_invalid
    );
    assert!(capture(&terminal, "data: {").protocol_invalid);
}
