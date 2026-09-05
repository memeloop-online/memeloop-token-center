use super::*;

use crate::api::limits::{
    MAX_SSE_FRAMED_BYTES_PER_NETWORK_CHUNK, MAX_SSE_FRAMES_PER_NETWORK_CHUNK,
};

#[test]
fn responses_sanitizer_forwards_complete_comment_heartbeats_and_rejects_eof_data() {
    for heartbeat in [
        b": heartbeat\n\n".as_slice(),
        b": heartbeat\r\r".as_slice(),
        b": heartbeat\r\n\r\n".as_slice(),
    ] {
        let mut sanitizer = codex_transport::ResponsesStreamingSanitizer::default();
        assert_eq!(sanitizer.push(heartbeat).unwrap().as_ref(), heartbeat);
        assert!(sanitizer.is_complete());
    }

    let split_heartbeat = b": split\r\n\r\n";
    let mut sanitizer = codex_transport::ResponsesStreamingSanitizer::default();
    let mut output = Vec::new();
    for byte in split_heartbeat {
        output.extend_from_slice(&sanitizer.push(&[*byte]).unwrap());
    }
    assert_eq!(output.as_slice(), split_heartbeat);

    let mut partial = codex_transport::ResponsesStreamingSanitizer::default();
    assert!(
        partial
            .push(b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-eof\"}}")
            .unwrap()
            .is_empty()
    );
    assert!(!partial.is_complete());
}

#[test]
fn responses_sanitizer_rejects_bad_terminal_ids_before_emitting_them() {
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
        let mut sanitizer = codex_transport::ResponsesStreamingSanitizer::default();
        let prior = sanitizer.push(created).unwrap();
        assert!(
            !String::from_utf8(prior.to_vec())
                .unwrap()
                .contains("response.completed")
        );
        assert_eq!(sanitizer.push(terminal), Err(expected));
    }
}

#[test]
fn buffered_responses_reject_unterminated_data_at_eof() {
    let partial = b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-buffered\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}";
    assert!(matches!(
        codex_transport::parse_buffered_sse_for_test(partial),
        Err("upstream_incomplete_response")
    ));
}

#[test]
fn responses_delivery_rejects_large_small_event_batches_before_delivery() {
    let event = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n";
    let chunk = event.repeat(MAX_SSE_FRAMES_PER_NETWORK_CHUNK + 1);
    assert!(chunk.len() < MAX_SSE_FRAMED_BYTES_PER_NETWORK_CHUNK);
    let mut capture = ResponsesSseCapture::for_responses();
    assert!(capture.push_delivery_frames(&chunk).is_empty());
    assert_eq!(capture.finish(), ResponsesSseOutcome::Incomplete);
}

#[test]
fn delivery_marks_trailing_unterminated_data_incomplete_even_after_done() {
    let mut capture = ResponsesSseCapture::for_delivery();
    let frames = capture.push_delivery_frames(b"data: [DONE]\n\ndata: truncated");
    assert_eq!(frames.len(), 1);
    assert!(!frames[0].billable);
    assert_eq!(capture.finish(), ResponsesSseOutcome::Incomplete);
}
