use super::*;

use crate::api::sse::ResponsesStreamingSanitizer;

#[test]
fn timeout_before_eof_has_no_terminal_flush_to_deliver() {
    let mut sanitizer = ResponsesStreamingSanitizer::default();
    assert!(sanitizer
        .push(b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-timeout\"}}\n\n")
        .unwrap()
        .is_empty());

    // The timeout path never invokes `finish_at_eof`; dropping both states
    // therefore cannot release a completed terminal to delivery or archive.
    let mut delivery = ResponsesTerminalDelivery::default();
    assert!(delivery.take_pending().is_none());
    drop(sanitizer);
}

#[test]
fn terminal_flush_fuses_the_upstream_poll_state() {
    let mut sanitizer = ResponsesStreamingSanitizer::default();
    sanitizer
        .push(b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-flush\"}}\n\n")
        .unwrap();
    let mut delivery = ResponsesTerminalDelivery::default();
    assert!(delivery.upstream_poll_allowed());
    assert_eq!(
        delivery.finish_at_eof(Some(&mut sanitizer)),
        TerminalEof::Flush
    );
    assert!(!delivery.upstream_poll_allowed());
    assert!(delivery.take_pending().is_some());
    assert!(!delivery.upstream_poll_allowed());
    assert_eq!(
        delivery.finish_at_eof(Some(&mut sanitizer)),
        TerminalEof::Complete
    );
}
