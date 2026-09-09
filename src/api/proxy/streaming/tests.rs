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
