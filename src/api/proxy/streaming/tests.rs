use super::*;

#[test]
fn native_kimi_chat_capture_preserves_documented_cache_usage_without_relaxing_openai() {
    let wire = include_bytes!("../../kimi_transport/fixtures/documented-chat-stream.sse");
    let mut kimi = chat_usage_capture(true);
    kimi.push(wire);
    let summary = kimi.finish_summary();
    assert!(matches!(
        summary.outcome,
        ResponsesSseOutcome::Completed { .. }
    ));
    assert!(!summary.usage_invalid);
    let usage = summary.usage.unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.output_tokens
        ),
        (7, 12, 13)
    );
    let mut strict = chat_usage_capture(false);
    strict.push(wire);
    assert!(strict.finish_summary().usage_invalid);
}

#[tokio::test]
async fn ready_upstream_evidence_wins_when_downstream_is_already_closed() {
    let (body_sender, body_receiver) = tokio::sync::mpsc::channel(1);
    drop(body_receiver);

    match poll_upstream_or_downstream_closed(&body_sender, std::future::ready("terminal")).await {
        DownstreamAwarePoll::Upstream {
            value,
            downstream_closed,
        } => {
            assert_eq!(value, "terminal");
            assert!(
                downstream_closed,
                "the caller must restrict follow-up polls to pending sanitizer evidence"
            );
        }
        DownstreamAwarePoll::DownstreamClosed => {
            panic!("an already-ready terminal item must win the cancellation race")
        }
    }
}

#[tokio::test]
async fn downstream_close_interrupts_a_pending_upstream_poll() {
    let (body_sender, body_receiver) = tokio::sync::mpsc::channel(1);
    drop(body_receiver);

    let result = tokio::time::timeout(
        Duration::from_millis(100),
        poll_upstream_or_downstream_closed(&body_sender, std::future::pending::<()>()),
    )
    .await
    .expect("receiver closure must wake the blocked poll without a wall-clock wait");
    assert!(matches!(result, DownstreamAwarePoll::DownstreamClosed));
}

#[tokio::test(start_paused = true)]
async fn progress_heartbeat_interrupts_a_quiet_upstream_before_stream_timeout() {
    let (body_sender, _body_receiver) = tokio::sync::mpsc::channel(1);
    let heartbeat_at = tokio::time::Instant::now() + Duration::from_secs(15);
    let poll = poll_upstream_downstream_or_progress_heartbeat(
        &body_sender,
        std::future::pending::<()>(),
        tokio::time::Instant::now() + Duration::from_secs(20),
        Some(heartbeat_at),
    );
    tokio::pin!(poll);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(15)).await;
    assert!(matches!(poll.await, StreamPoll::ProgressHeartbeat));
}

#[test]
fn observed_downstream_close_keeps_cancellation_attribution() {
    for error in [
        "upstream_stream",
        "upstream_stream_read_error",
        "upstream_timeout",
        "upstream_invalid_response",
        "upstream_incomplete_response",
    ] {
        assert_eq!(
            transport_error_with_downstream_precedence(true, error),
            "downstream_disconnected"
        );
        assert_eq!(
            transport_error_with_downstream_precedence(false, error),
            error
        );
    }
}

#[test]
fn send_disconnect_only_finishes_nonempty_pending_terminal_delivery() {
    let completed = b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-ready\",\"usage\":{\"input_tokens\":3,\"output_tokens\":7}}}\n\n";
    let mut sanitizer = crate::api::sse::ResponsesStreamingSanitizer::default();
    assert!(sanitizer.push(completed).unwrap().is_empty());
    assert!(pending_delivery_can_advance(
        Some(&sanitizer),
        completed.len()
    ));
    let mut transport_error = Some("downstream_disconnected");
    let mut downstream_closed = false;
    assert!(resume_pending_delivery_after_send_disconnect(
        &mut transport_error,
        &mut downstream_closed,
        Some(&sanitizer)
    ));
    assert_eq!(transport_error, None);
    assert!(downstream_closed);
    assert!(
        !pending_delivery_can_advance(Some(&sanitizer), 0),
        "empty ready chunks cannot advance framing and must not starve cancellation"
    );
    transport_error = Some("delivery_state");
    downstream_closed = false;
    assert!(!resume_pending_delivery_after_send_disconnect(
        &mut transport_error,
        &mut downstream_closed,
        Some(&sanitizer)
    ));
    assert_eq!(transport_error, Some("delivery_state"));
    assert!(!downstream_closed);

    assert!(!sanitizer.finish().unwrap().is_empty());
    transport_error = Some("downstream_disconnected");
    assert!(!resume_pending_delivery_after_send_disconnect(
        &mut transport_error,
        &mut downstream_closed,
        Some(&sanitizer)
    ));
}

#[tokio::test]
async fn archive_eof_owner_keeps_the_body_open_until_settlement_handoff_finishes() {
    let (body_sender, body_receiver) = tokio::sync::mpsc::channel(1);
    let (settlement_sender, settlement_receiver) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let owner = tokio::spawn(hold_response_eof_until_archive_settles(
        settlement_receiver,
        body_sender.clone(),
        proxy_diagnostics::Context::new(),
    ));
    let settlement =
        crate::response_archive_spool::ResponseArchiveSettlement::pending_for_test(released);
    assert!(settlement_sender.send(Some(settlement)).is_ok());
    drop(body_sender);
    tokio::task::yield_now().await;
    assert!(
        !body_receiver.is_closed(),
        "the independently owned sender must keep graceful HTTP drain open"
    );

    release.send(()).unwrap();
    owner.await.unwrap();
    assert!(body_receiver.is_closed());
}

fn batch_limit_chunk() -> Vec<u8> {
    b"data: {}\n\n".repeat(crate::api::limits::MAX_SSE_FRAMES_PER_NETWORK_CHUNK + 1)
}

#[test]
fn batch_limit_without_prior_invalidity_has_no_observed_protocol_violation() {
    let mut capture = ResponsesSseCapture::for_responses();
    assert!(matches!(
        capture.push_delivery_frames(&batch_limit_chunk()),
        Err(crate::api::sse::SseFramerRejection::BatchLimit)
    ));
    let summary = capture.finish_summary();
    assert!(!summary.observed_protocol_invalid);
    assert!(summary.protocol_invalid);
}

#[test]
fn batch_limit_preserves_a_prior_semantic_protocol_violation() {
    let mut capture = ResponsesSseCapture::for_responses();
    capture.push_delivery_frames(b"data: not-json\n\n").unwrap();
    assert!(matches!(
        capture.push_delivery_frames(&batch_limit_chunk()),
        Err(crate::api::sse::SseFramerRejection::BatchLimit)
    ));
    let summary = capture.finish_summary();
    assert!(summary.observed_protocol_invalid);
    assert!(summary.protocol_invalid);
}

#[test]
fn event_limit_is_observed_protocol_invalidity() {
    let mut capture = ResponsesSseCapture::for_responses();
    let oversized = vec![b'x'; crate::api::limits::MAX_RESPONSES_SSE_EVENT_BYTES + 1];
    assert!(matches!(
        capture.push_delivery_frames(&oversized),
        Err(crate::api::sse::SseFramerRejection::EventLimit)
    ));
    let summary = capture.finish_summary();
    assert!(summary.observed_protocol_invalid);
    assert!(summary.protocol_invalid);
}

#[test]
fn strict_chat_valid_prefix_is_not_confused_with_missing_final_usage() {
    let mut capture = ResponsesSseCapture::for_openai_chat_usage();
    let frames = capture.push_delivery_frames(
        b"data: {\"id\":\"chatcmpl-probe\",\"object\":\"chat.completion.chunk\",\"model\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"valid output\"},\"finish_reason\":null}]}\n\n",
    ).unwrap();
    assert!(frames.iter().any(|frame| frame.billable));
    assert!(capture.can_confirm_probe_delivery());
    let summary = capture.finish_summary();
    assert!(
        summary.usage_invalid,
        "final usage is correctly incomplete until DONE"
    );
    assert!(
        summary.observed_protocol_invalid,
        "usage invalidity discovered during finalization remains independent of EOF truncation"
    );
}

#[test]
fn anthropic_message_stop_is_held_until_archive_barrier() {
    let mut capture = ResponsesSseCapture::for_delivery();
    let mut held = delivery::TerminalFrames::default();
    let delivery = capture_sse_delivery(
        Some(&mut capture),
        Bytes::from_static(
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hello\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ),
        false,
    ).unwrap();
    let mut immediate = Vec::new();
    for frame in delivery.frames {
        if let Some(frame) = held.hold(frame).unwrap() {
            immediate.extend_from_slice(&frame.bytes);
        }
    }
    assert!(std::str::from_utf8(&immediate).unwrap().contains("hello"));
    assert!(
        !std::str::from_utf8(&immediate)
            .unwrap()
            .contains("message_stop")
    );
    assert!(!held.is_empty());
    assert!(matches!(
        capture.finish_summary().outcome,
        ResponsesSseOutcome::Completed { .. }
    ));
    assert!(held.take()[0].terminal);
}

#[test]
fn invalid_terminals_use_protocol_specific_fixed_errors_without_success() {
    for protocol in [
        Protocol::OpenAiChat,
        Protocol::OpenAiResponses,
        Protocol::AnthropicMessages,
    ] {
        let bytes = delivery::invalid_terminal_failure(protocol);
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.contains("error"));
        assert!(!text.contains("[DONE]"));
        assert!(!text.contains("response.completed"));
        assert!(!text.contains("message_stop"));
    }
}

#[test]
fn capture_preserves_crlf_continuation_but_drops_post_terminal_control_for_spool() {
    let mut capture = ResponsesSseCapture::for_delivery();
    let chunks = [
        Bytes::from_static(b"data: [DONE]\r\n\r"),
        Bytes::from_static(b"\n: heartbeat\n\n"),
    ];
    let mut archived = Vec::new();
    for chunk in &chunks {
        let delivery = capture_sse_delivery(Some(&mut capture), chunk.clone(), false).unwrap();
        for frame in delivery.frames {
            archived.extend_from_slice(&frame.bytes);
        }
    }
    assert_eq!(archived, b"data: [DONE]\r\n\r\n");
}

#[test]
fn capture_preserves_bare_cr_and_nine_frame_burst_order_for_spool() {
    let mut capture = ResponsesSseCapture::for_delivery();
    let burst = (0..9)
        .map(|i| format!("data: {{\"delta\":\"{i}\"}}\r\r"))
        .collect::<String>();
    let delivery =
        capture_sse_delivery(Some(&mut capture), Bytes::from(burst.clone()), false).unwrap();
    assert_eq!(delivery.frames.len(), 9);
    let archived: Vec<u8> = delivery
        .frames
        .iter()
        .flat_map(|frame| frame.bytes.iter().copied())
        .collect();
    assert_eq!(archived, burst.as_bytes());
}

#[test]
fn terminal_barrier_holds_done_and_split_crlf_but_not_text() {
    let mut capture = ResponsesSseCapture::for_delivery();
    let mut held = delivery::TerminalFrames::default();
    let mut delivered = Vec::new();
    let mut archive = Vec::new();
    for chunk in [
        Bytes::from_static(b"data: {\"delta\":\"text\"}\n\ndata: [DONE]\r\n\r"),
        Bytes::from_static(b"\n"),
    ] {
        let delivery = capture_sse_delivery(Some(&mut capture), chunk, false).unwrap();
        for frame in delivery.frames {
            archive.extend_from_slice(&frame.bytes);
            if let Some(frame) = held.hold(frame).unwrap() {
                delivered.extend_from_slice(&frame.bytes);
            }
        }
    }
    assert_eq!(delivered, b"data: {\"delta\":\"text\"}\n\n");
    assert!(matches!(
        capture.finish_summary().outcome,
        ResponsesSseOutcome::Completed { .. }
    ));
    // This release is called only after the seal/gap acknowledgement in the
    // production lifecycle; archive captures the exact same CRLF wire bytes.
    for frame in held.take() {
        delivered.extend_from_slice(&frame.bytes);
    }
    assert_eq!(delivered, archive);
    assert!(delivered.ends_with(b"data: [DONE]\r\n\r\n"));
    assert!(held.take().is_empty());
}

#[test]
fn terminal_barrier_bounds_the_tail_and_preserves_terminal_billing_class() {
    let mut held = delivery::TerminalFrames::default();
    assert!(
        held.hold(SseDeliveryFrame {
            bytes: Bytes::from_static(b"terminal-output"),
            billable: true,
            terminal: true,
        })
        .unwrap()
        .is_none()
    );
    let frames = held.take();
    assert!(frames[0].billable);
    assert!(
        held.hold(SseDeliveryFrame {
            bytes: Bytes::from(vec![
                b'x';
                crate::api::limits::MAX_SSE_FRAMED_BYTES_PER_NETWORK_CHUNK
                    + 1
            ]),
            billable: false,
            terminal: true,
        })
        .is_err()
    );
}
