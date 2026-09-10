use super::*;

#[test]
fn strict_chat_valid_prefix_is_not_confused_with_missing_final_usage() {
    let mut capture = ResponsesSseCapture::for_openai_chat_usage();
    let frames = capture.push_delivery_frames(
        b"data: {\"id\":\"chatcmpl-probe\",\"object\":\"chat.completion.chunk\",\"model\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"valid output\"},\"finish_reason\":null}]}\n\n",
    ).unwrap();
    assert!(frames.iter().any(|frame| frame.billable));
    assert!(capture.can_confirm_probe_delivery());
    assert!(
        capture.finish_summary().usage_invalid,
        "final usage is correctly incomplete until DONE"
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
