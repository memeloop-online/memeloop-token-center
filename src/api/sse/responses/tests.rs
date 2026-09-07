use super::*;

use crate::api::limits::{
    MAX_RESPONSES_SSE_EVENT_BYTES, MAX_RESPONSES_SSE_TERMINAL_HOLD_BYTES, MAX_SSE_FIELDS_PER_EVENT,
    MAX_SSE_FRAMED_BYTES_PER_NETWORK_CHUNK, MAX_SSE_FRAMES_PER_NETWORK_CHUNK,
};

fn event_with_field_count(fields: usize) -> Vec<u8> {
    let mut event = b"x\n".repeat(fields);
    event.push(b'\n');
    event
}

#[test]
fn sanitizer_redacts_failures_and_rejects_terminal_conflicts() {
    let failed = concat!(
        "event: response.failed\n",
        "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"provider-secret\",\"token\":\"secret-token\"}}}\n\n",
        ": Authorization: failure-comment-secret\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"post-terminal-secret\"}\n\n",
        "data: [DONE]\n\n"
    );
    let mut sanitizer = ResponsesStreamingSanitizer::default();
    let mut output = Vec::new();
    for byte in failed.as_bytes() {
        output.extend_from_slice(&sanitizer.push(&[*byte]).unwrap());
    }
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("upstream request failed"));
    for secret in [
        "provider-secret",
        "secret-token",
        "failure-comment-secret",
        "post-terminal-secret",
    ] {
        assert!(!output.contains(secret));
    }

    let mut conflict = ResponsesStreamingSanitizer::default();
    assert!(conflict
        .push(
            b"event: response.failed\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n",
        )
        .is_err());
    let mut after_completed = ResponsesStreamingSanitizer::default();
    after_completed
        .push(
            b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-complete\"}}\n\n",
        )
        .unwrap();
    assert!(
        after_completed
            .push(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"secret\"}\n\n")
            .is_err()
    );

    let mut unknown_type = ResponsesStreamingSanitizer::default();
    assert!(
        unknown_type
            .push(b"data: {\"type\":\"not-responses\",\"secret\":\"hidden\"}\n\n")
            .is_err()
    );

    let mut unknown_field = ResponsesStreamingSanitizer::default();
    let mut output = unknown_field
        .push(
            concat!(
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp\"}}\n\n",
                "<html>post-admission-secret</html>\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp\"}}\n\n"
            )
            .as_bytes(),
        )
        .unwrap()
        .to_vec();
    output.extend_from_slice(&unknown_field.finish().unwrap());
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("response.created"));
    assert!(output.contains("response.completed"));
    assert!(!output.contains("post-admission-secret"));

    let mut duplicate_event = ResponsesStreamingSanitizer::default();
    assert!(
        duplicate_event
            .push(
                concat!(
                    "event: provider-secret\n",
                    "event: response.created\n",
                    "data: {\"type\":\"response.created\",\"response\":{}}\n\n"
                )
                .as_bytes(),
            )
            .is_err()
    );

    let mut done = ResponsesStreamingSanitizer::default();
    done.push(b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-done\"}}\n\n")
        .unwrap();
    assert!(
        done.push(b"event: provider-secret\ndata: [DONE]\n\n")
            .unwrap()
            .is_empty()
    );
    let output = done.finish().unwrap();
    assert!(output.ends_with(b"data: [DONE]\n\n"));
}

#[test]
fn failed_terminal_drops_a_bare_secret_event_before_done() {
    let stream = concat!(
        "event: response.failed\n",
        "data: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp-failed\",\"error\":{\"message\":\"provider-secret\"}}}\n\n",
        "event: Authorization-Bearer-bare-event-secret\n\n",
        "data: [DONE]\n\n"
    );
    let mut sanitizer = ResponsesStreamingSanitizer::default();
    let mut output = Vec::new();
    for chunk in stream.as_bytes().chunks(7) {
        output.extend_from_slice(&sanitizer.push(chunk).unwrap());
    }
    output.extend_from_slice(&sanitizer.finish().unwrap());
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("upstream request failed"));
    assert!(output.contains("data: [DONE]"));
    assert!(!output.contains("provider-secret"));
    assert!(!output.contains("bare-event-secret"));
}

#[test]
fn sanitizer_preserves_bare_cr_and_cross_chunk_crlf_boundaries() {
    let created =
        b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-boundary\"}}\r\r";

    let mut bare_cr = ResponsesStreamingSanitizer::default();
    assert_eq!(bare_cr.push(created).unwrap().as_ref(), created);
    assert!(bare_cr.saw_protocol_event());
    assert!(bare_cr.is_complete());

    let mut split_crlf = ResponsesStreamingSanitizer::default();
    let mut output = split_crlf.push(created).unwrap().to_vec();
    assert!(split_crlf.saw_protocol_event());
    output.extend_from_slice(&split_crlf.push(b"\n").unwrap());
    let mut expected = created.to_vec();
    expected.push(b'\n');
    assert_eq!(output, expected);
    assert!(split_crlf.is_complete());
}

#[test]
fn sanitizer_bounds_each_event_not_the_network_chunk() {
    // Keep the complete batch within the independent decoder-product caps.
    // The property under test is that an otherwise bounded network chunk may
    // exceed the per-event cap, not that it may bypass the batch cap.
    let mut event =
        b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp\"},\"padding\":\""
            .to_vec();
    event.extend_from_slice(&[b'x'; 512]);
    event.extend_from_slice(b"\"}\n\n");
    let repeats = MAX_RESPONSES_SSE_EVENT_BYTES / event.len() + 1;
    let network_chunk = event.repeat(repeats);
    assert!(network_chunk.len() > MAX_RESPONSES_SSE_EVENT_BYTES);
    assert!(network_chunk.len() <= MAX_SSE_FRAMED_BYTES_PER_NETWORK_CHUNK);
    assert!(repeats <= MAX_SSE_FRAMES_PER_NETWORK_CHUNK);
    let mut sanitizer = ResponsesStreamingSanitizer::default();
    let output = sanitizer.push(&network_chunk).unwrap();
    assert_eq!(output.as_ref(), network_chunk);
    assert!(sanitizer.is_complete());

    let mut oversized = ResponsesStreamingSanitizer::default();
    let first = vec![b'x'; MAX_RESPONSES_SSE_EVENT_BYTES / 2];
    let second = vec![b'x'; MAX_RESPONSES_SSE_EVENT_BYTES / 2 + 1];
    assert!(oversized.push(&first).is_ok());
    assert_eq!(
        oversized.push(&second),
        Err("upstream_response_event_too_large")
    );
}

#[test]
fn sanitizer_accepts_exact_field_limit_and_rejects_the_next_without_delivery() {
    let mut sanitizer = ResponsesStreamingSanitizer::default();
    assert!(
        sanitizer
            .push(&event_with_field_count(MAX_SSE_FIELDS_PER_EVENT))
            .is_ok()
    );

    let mut sanitizer = ResponsesStreamingSanitizer::default();
    assert_eq!(
        sanitizer.push(&event_with_field_count(MAX_SSE_FIELDS_PER_EVENT + 1)),
        Err("upstream_response_event_too_large")
    );
}

#[test]
fn sanitizer_accepts_only_blank_lines_after_a_framed_terminal_event() {
    for stream in [
        b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-terminal\"}}\n\n\n".as_slice(),
        b"event: response.completed\r\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-terminal\"}}\r\n\r\n\r\n".as_slice(),
    ] {
        for split in 0..=stream.len() {
            let mut sanitizer = ResponsesStreamingSanitizer::default();
            let mut output = sanitizer.push(&stream[..split]).unwrap().to_vec();
            output.extend_from_slice(&sanitizer.push(&stream[split..]).unwrap());
            output.extend_from_slice(&sanitizer.finish().unwrap());
            assert!(String::from_utf8(output).unwrap().contains("response.completed"));
            assert!(sanitizer.is_complete(), "split at byte {split}");
        }
    }

    let mut partial = ResponsesStreamingSanitizer::default();
    partial
        .push(b"data: {\"type\":\"response.created\"}")
        .unwrap();
    assert!(!partial.is_complete());

    let mut trailing_partial = ResponsesStreamingSanitizer::default();
    trailing_partial
        .push(
            b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-terminal\"}}\n\n\ndata:",
        )
        .unwrap();
    assert!(!trailing_partial.is_complete());
}

#[test]
fn idle_comments_are_redacted_immediately_and_control_tails_keep_terminal_complete() {
    let mut sanitizer = ResponsesStreamingSanitizer::default();
    assert_eq!(
        sanitizer
            .push(b": Authorization: provider-secret\r\n")
            .unwrap()
            .as_ref(),
        b": heartbeat\r\n"
    );
    assert!(sanitizer.is_complete());

    let completed =
        b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-comment\"}}\n\n";
    assert!(sanitizer.push(completed).unwrap().is_empty());
    assert!(
        sanitizer
            .push(b": provider diagnostic\n")
            .unwrap()
            .is_empty()
    );
    assert!(
        sanitizer
            .push(b"id: ignored\nretry: 1\n")
            .unwrap()
            .is_empty()
    );
    let held = sanitizer.finish().unwrap();
    assert!(String::from_utf8_lossy(&held).contains("response.completed"));
    assert!(!String::from_utf8_lossy(&held).contains("provider diagnostic"));
}

#[test]
fn terminal_hold_releases_only_after_complete_eof_framing_and_is_bounded() {
    let completed = b"event: response.completed\r\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-hold\"}}\r\n\r\n";
    let mut sanitizer = ResponsesStreamingSanitizer::default();
    assert!(sanitizer.push(completed).unwrap().is_empty());
    assert!(sanitizer.push(b"data: [DONE]\r\n\r\n").unwrap().is_empty());
    let terminal = sanitizer.finish().unwrap();
    assert!(String::from_utf8_lossy(&terminal).contains("response.completed"));
    assert!(terminal.ends_with(b"data: [DONE]\r\n\r\n"));

    let mut trailing = ResponsesStreamingSanitizer::default();
    assert!(trailing.push(completed).unwrap().is_empty());
    assert!(trailing.push(b"data: truncated").unwrap().is_empty());
    assert_eq!(trailing.finish(), Err("upstream_incomplete_response"));

    let mut hold = ResponseTerminalHold::default();
    hold.begin();
    assert_eq!(
        hold.append(&vec![b'x'; MAX_RESPONSES_SSE_TERMINAL_HOLD_BYTES + 1]),
        Err("upstream_response_terminal_too_large")
    );
}

#[test]
fn sanitizer_rejects_bad_ids_and_bare_lifecycle_events_before_terminal_delivery() {
    let created = b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-a\"}}\n\n";
    for (terminal, expected) in [
        (
            b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-b\"}}\n\n"
                .as_slice(),
            "upstream_invalid_response",
        ),
        (
            b"data: {\"type\":\"response.completed\",\"response\":{}}\n\n".as_slice(),
            "upstream_incomplete_response",
        ),
    ] {
        let mut sanitizer = ResponsesStreamingSanitizer::default();
        let prior = sanitizer.push(created).unwrap();
        assert!(
            !String::from_utf8(prior.to_vec())
                .unwrap()
                .contains("response.completed")
        );
        assert_eq!(sanitizer.push(terminal), Err(expected));
    }

    let mut bare = ResponsesStreamingSanitizer::default();
    assert_eq!(
        bare.push(b"event: response.completed\n\n"),
        Err("upstream_invalid_response")
    );

    for lifecycle in [
        "response.queued",
        "response.created",
        "response.in_progress",
    ] {
        let missing_id = format!("data: {{\"type\":\"{lifecycle}\",\"response\":{{}}}}\n\n");
        let mut sanitizer = ResponsesStreamingSanitizer::default();
        assert_eq!(
            sanitizer.push(missing_id.as_bytes()),
            Err("upstream_incomplete_response")
        );

        let non_string_id =
            format!("data: {{\"type\":\"{lifecycle}\",\"response\":{{\"id\":42}}}}\n\n");
        let mut sanitizer = ResponsesStreamingSanitizer::default();
        assert_eq!(
            sanitizer.push(non_string_id.as_bytes()),
            Err("upstream_invalid_response")
        );
    }

    let mut item_before_id = ResponsesStreamingSanitizer::default();
    assert_eq!(
        item_before_id.push(
            b"data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"item-secret\"}}\n\n",
        ),
        Err("upstream_invalid_response")
    );
}

#[test]
fn sanitizer_rejection_stages_are_static_and_content_free() {
    let cases = [
        (
            b"data: {not-json}\n\n".as_slice(),
            "upstream_invalid_response",
            "json",
        ),
        (
            b"data: {\"type\":\"not-responses\",\"provider_detail\":\"must-not-log\"}\n\n"
                .as_slice(),
            "upstream_invalid_response",
            "payload_namespace",
        ),
        (
            b"data: []\n\n".as_slice(),
            "upstream_invalid_response",
            "payload_shape",
        ),
        (
            b"data: {}\n\n".as_slice(),
            "upstream_invalid_response",
            "payload_schema",
        ),
        (
            b"data: {\"type\":false}\n\n".as_slice(),
            "upstream_invalid_response",
            "payload_schema",
        ),
        (
            b"event: response.created\ndata: {\"type\":\"response.queued\",\"response\":{\"id\":\"resp-mismatch\"}}\n\n"
                .as_slice(),
            "upstream_invalid_response",
            "event_type_mismatch",
        ),
        (
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"must-not-log\"}\n\n"
                .as_slice(),
            "upstream_invalid_response",
            "response_identity",
        ),
    ];
    for (stream, code, stage) in cases {
        let mut sanitizer = ResponsesStreamingSanitizer::default();
        assert_eq!(sanitizer.push(stream), Err(code));
        assert_eq!(sanitizer.last_rejection_stage(), stage);
        assert!(!sanitizer.last_rejection_stage().contains("must-not-log"));
    }

    let mut eof = ResponsesStreamingSanitizer::default();
    eof.push(b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-eof\"}}")
        .unwrap();
    assert_eq!(eof.finish(), Err("upstream_incomplete_response"));
    assert_eq!(eof.last_rejection_stage(), "eof_incomplete");
}
