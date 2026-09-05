use super::*;

use crate::api::limits::{
    MAX_SSE_FIELDS_PER_EVENT, MAX_SSE_FRAMED_BYTES_PER_NETWORK_CHUNK,
    MAX_SSE_FRAMES_PER_NETWORK_CHUNK,
};

fn event_with_field_count(fields: usize) -> Vec<u8> {
    let mut event = b"x\n".repeat(fields);
    event.push(b'\n');
    event
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
fn field_overflow_is_an_event_limit_with_no_delivery_prefix() {
    let overflow = event_with_field_count(MAX_SSE_FIELDS_PER_EVENT + 1);
    let mut capture = ResponsesSseCapture::for_delivery();
    assert!(capture.push_delivery_frames(&overflow).is_empty());
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
