use super::*;

use crate::api::limits::MAX_SSE_FRAMED_BYTES_PER_NETWORK_CHUNK;

#[tokio::test]
async fn archive_batch_queue_accepts_control_and_content_with_unpolled_receiver() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let frames = [
        SseDeliveryFrame {
            bytes: Bytes::from_static(b": heartbeat\n\n"),
            billable: false,
        },
        SseDeliveryFrame {
            bytes: Bytes::from_static(b"data: {\"delta\":\"ok\"}\n\n"),
            billable: true,
        },
    ];

    // This is a queue-level gate with the production capacity of one; a
    // frame-at-a-time send would reject the second frame before a writer polls.
    assert!(try_queue_response_archive_batch(&sender, &frames).is_ok());
    let batch = receiver.recv().await.expect("one archive batch");
    assert_eq!(
        batch.chunks,
        vec![
            Bytes::from_static(b": heartbeat\n\n"),
            Bytes::from_static(b"data: {\"delta\":\"ok\"}\n\n"),
        ]
    );
}

#[test]
fn archive_batch_reuses_the_framed_byte_budget() {
    let frames = [
        SseDeliveryFrame {
            bytes: Bytes::from(vec![b'x'; MAX_SSE_FRAMED_BYTES_PER_NETWORK_CHUNK]),
            billable: false,
        },
        SseDeliveryFrame {
            bytes: Bytes::from_static(b"\n"),
            billable: true,
        },
    ];
    assert!(matches!(
        ResponseArchiveBatch::from_delivery_frames(&frames),
        Err(ResponseArchiveBatchError::BatchLimit)
    ));
}
