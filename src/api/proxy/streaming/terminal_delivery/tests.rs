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

#[tokio::test]
async fn eof_pending_and_slow_receiver_preserve_live_terminal_charge() {
    let budget = crate::gateway_body::memory::ProxyMemoryBudget::new(1024 * 1024);
    let request = budget.reservation();
    let allocation = std::sync::Arc::new(crate::gateway_body::memory::SseAllocationBudget::new(
        request.try_reserve_sse_streaming(0).unwrap(),
    ));
    let mut sanitizer = ResponsesStreamingSanitizer::default();
    sanitizer.set_allocation_budget(allocation.clone());
    sanitizer
        .push(b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-held\"}}\n\n")
        .unwrap();
    let mut delivery = ResponsesTerminalDelivery::default();
    assert_eq!(
        delivery.finish_at_eof(Some(&mut sanitizer)),
        TerminalEof::Flush
    );
    assert!(delivery.retained_bytes() > 0);
    assert!(
        allocation.reconcile_retained(sanitizer.retained_capacity() + delivery.retained_bytes())
    );
    assert!(budget.snapshot().0 > 0);
    let terminal = allocation
        .retain_output(delivery.take_pending().unwrap())
        .unwrap();
    let last_clone = terminal.clone();
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    sender.send(terminal).await.unwrap();
    drop(sanitizer);
    drop(allocation);
    assert!(budget.snapshot().0 > 0);
    let received = receiver.recv().await.unwrap();
    assert!(
        std::str::from_utf8(&received)
            .unwrap()
            .contains("response.completed")
    );
    drop(received);
    assert!(budget.snapshot().0 > 0);
    drop(last_clone);
    assert_eq!(budget.snapshot().0, 0);
}
