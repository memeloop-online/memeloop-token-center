use super::support::*;
use super::*;

#[test]
fn stateful_sse_delivery_framer_keeps_split_comments_and_done_nonbillable() {
    let mut capture = ResponsesSseCapture::for_openai_chat_usage();
    assert!(capture.push_delivery_frames(b": pi").is_empty());
    assert!(capture.push_delivery_frames(b"ng\r\n").is_empty());
    let frames = capture.push_delivery_frames(b"\r\n: two\n\n");
    assert_eq!(frames.len(), 2);
    assert!(frames.iter().all(|frame| !frame.billable));
    assert_eq!(frames[0].bytes, Bytes::from_static(b": ping\r\n\r\n"));
    assert_eq!(frames[1].bytes, Bytes::from_static(b": two\n\n"));
    let frames = capture.push_delivery_frames(done().as_bytes());
    assert_eq!(frames.len(), 1);
    assert!(!frames[0].billable);
    let frames = capture.push_delivery_frames(&[]);
    assert!(frames.is_empty());
}

#[test]
fn shared_delivery_framer_handles_all_line_endings_without_eof_dispatch() {
    for heartbeat in [
        b": lf\n\n".as_slice(),
        b": cr\r\r".as_slice(),
        b": crlf\r\n\r\n".as_slice(),
    ] {
        let mut capture = ResponsesSseCapture::for_delivery();
        let frames = capture.push_delivery_frames(heartbeat);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].bytes.as_ref(), heartbeat);
        assert!(!frames[0].billable);
    }

    let mut truncated = ResponsesSseCapture::for_responses();
    assert!(
        truncated
            .push_delivery_frames(
                b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-eof\"}}",
            )
            .is_empty()
    );
    assert_eq!(truncated.finish(), ResponsesSseOutcome::Incomplete);
}

#[test]
fn openai_chat_admission_rejects_n_other_than_one() {
    assert!(validate_openai_chat_choice_count(&json!({})).is_ok());
    assert!(validate_openai_chat_choice_count(&json!({"n": 1})).is_ok());
    for n in [json!(0), json!(2), json!("1"), json!(1.0)] {
        assert!(validate_openai_chat_choice_count(&json!({"n": n})).is_err());
    }
}

#[test]
fn chat_usage_contract_requires_an_explicit_account_or_route_opt_in() {
    assert!(!ChatSseUsageContract::from_route_config(&json!({})).requires_terminal_usage());
    assert!(
        !ChatSseUsageContract::from_route_config(&json!({
            "stream_usage_contract": "unrecognized-compatible-dialect"
        }))
        .requires_terminal_usage()
    );
    assert!(
        ChatSseUsageContract::from_route_config(&json!({
            "stream_usage_contract": "openai-chat-usage-only"
        }))
        .requires_terminal_usage()
    );
}

#[test]
fn responses_queued_events_are_control_frames() {
    let mut capture = ResponsesSseCapture::for_responses();
    let frames = capture.push_delivery_frames(
        b"event: response.queued\ndata: {\"type\":\"response.queued\",\"response\":{\"id\":\"resp-queued\"}}\n\n",
    );
    assert_eq!(frames.len(), 1);
    assert!(!frames[0].billable);
}

#[test]
fn responses_completed_frames_are_billable_only_with_output_or_usage() {
    for (data, billable) in [
        (
            b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-empty\"}}\n\n"
                .as_slice(),
            false,
        ),
        (
            b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-output\",\"output\":[{\"type\":\"message\"}]}}\n\n"
                .as_slice(),
            true,
        ),
        (
            b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-usage\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n"
                .as_slice(),
            true,
        ),
    ] {
        let mut capture = ResponsesSseCapture::for_responses();
        let frames = capture.push_delivery_frames(data);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].billable, billable);
    }
}

#[test]
fn responses_safe_failures_are_billable_only_for_compatible_routes() {
    let event = b"event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp-failed\"}}\n\n";
    for (mut capture, billable) in [
        (ResponsesSseCapture::for_responses(), true),
        (ResponsesSseCapture::for_codex_responses(), false),
    ] {
        let frames = capture.push_delivery_frames(event);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].billable, billable);
    }
}

#[test]
fn typed_chat_dto_accepts_standard_metadata_keys() {
    let data = serde_json::to_vec(&json!({
        "id": "chatcmpl-standard-metadata",
        "object": "chat.completion.chunk",
        "model": "compatible-chat-model",
        "choices": [],
        "obfuscation": null,
        "moderation": null,
        "service_tier": "priority",
        "usage": {
            "prompt_tokens": 3,
            "completion_tokens": 2,
            "total_tokens": 5,
            "prompt_tokens_details": {
                "cached_tokens": null,
                "cache_write_tokens": 0,
                "audio_tokens": null,
                "image_tokens": null,
                "text_tokens": 3,
            },
            "completion_tokens_details": {
                "accepted_prediction_tokens": null,
                "audio_tokens": null,
                "reasoning_tokens": null,
                "rejected_prediction_tokens": null,
                "text_tokens": 2,
            },
        },
    }))
    .unwrap();
    assert!(super::super::super::chat_sse_usage::canonical_chat_chunk_is_accepted(&data));
}

#[test]
fn chat_no_op_preambles_are_control_frames() {
    for delta in [json!({}), json!({"role": "assistant", "content": ""})] {
        let mut capture = ResponsesSseCapture::for_openai_chat_usage();
        let frames = capture.push_delivery_frames(
            chat_chunk(
                "chatcmpl-control-preamble",
                json!([{
                    "index": 0,
                    "delta": delta,
                    "finish_reason": null,
                }]),
                None,
            )
            .as_bytes(),
        );
        assert_eq!(frames.len(), 1);
        assert!(!frames[0].billable);
    }
}

#[test]
fn chat_sse_requires_complete_canonical_usage_and_one_consistent_chat_id() {
    let valid = [
        chat_content("chatcmpl-one"),
        chat_finish("chatcmpl-one"),
        chat_usage_only(
            "chatcmpl-one",
            json!({
                "prompt_tokens": 9,
                "completion_tokens": 3,
                "total_tokens": 12,
                "prompt_tokens_details": {"cached_tokens": 4},
            }),
        ),
        done().to_owned(),
    ]
    .concat();
    let mut capture = ResponsesSseCapture::for_openai_chat_usage();
    for chunk in valid.as_bytes().chunks(7) {
        capture.push(chunk);
    }
    let summary = capture.finish_summary();
    assert_eq!(
        summary.outcome,
        ResponsesSseOutcome::Completed { response_id: None }
    );
    assert_eq!(
        summary.usage,
        Some(TokenUsage {
            input_tokens: 5,
            cached_input_tokens: 4,
            output_tokens: 3,
            ..TokenUsage::default()
        }),
    );
    assert!(!summary.usage_invalid);

    let switched_id = [
        chat_content("chatcmpl-one"),
        chat_finish("chatcmpl-one"),
        chat_usage_only("chatcmpl-two", usage(9, 3, 12)),
        done().to_owned(),
    ]
    .concat();
    let unfinished_choice = [
        chat_content("chatcmpl-one"),
        chat_chunk(
            "chatcmpl-one",
            json!([{"index": 1, "delta": {"content": "also"}, "finish_reason": null}]),
            None,
        ),
        chat_finish("chatcmpl-one"),
        chat_usage_only("chatcmpl-one", usage(29, 7, 36)),
        done().to_owned(),
    ]
    .concat();
    for invalid in [
        valid.replace("\"total_tokens\":12", "\"total_tokens\":13"),
        valid.replace("\"prompt_tokens\":9", "\"input_tokens\":9"),
        valid.replace("\"completion_tokens\":3", "\"output_tokens\":3"),
        valid.replace(
            "\"prompt_tokens\":9",
            "\"prompt_tokens\":9,\"input_tokens\":9",
        ),
        valid.replace(
            "\"prompt_tokens\":9",
            "\"prompt_tokens\":9,\"prompt_tokens\":8",
        ),
        valid.replace(
            "\"cached_tokens\":4",
            "\"cached_tokens\":4,\"cache_read_input_tokens\":4",
        ),
        valid.replace("\"id\":\"chatcmpl-one\",", ""),
        switched_id,
        unfinished_choice,
        valid.replace("data: [DONE]\n\n", ""),
    ] {
        let mut capture = ResponsesSseCapture::for_openai_chat_usage();
        capture.push(invalid.as_bytes());
        assert!(capture.finish_summary().usage_invalid);
    }
}
