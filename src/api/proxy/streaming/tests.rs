use super::*;

#[test]
fn capture_preserves_crlf_continuation_and_following_control_frame_for_spool() {
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
    assert_eq!(archived, chunks.concat());
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
