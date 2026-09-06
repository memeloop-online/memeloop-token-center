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

#[tokio::test]
async fn deferred_bare_cr_and_next_event_share_one_archive_slot_in_order() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let mut deferred = DeferredResponseArchive::default();
    let first = [SseDeliveryFrame {
        bytes: Bytes::from_static(b"data: {\"delta\":\"first\"}\r\r"),
        billable: true,
    }];
    deferred.queue(&sender, &first, true, false).unwrap();
    assert!(receiver.try_recv().is_err());

    let second = [SseDeliveryFrame {
        bytes: Bytes::from_static(b"data: {\"delta\":\"second\"}\r\r"),
        billable: true,
    }];
    deferred.queue(&sender, &second, false, false).unwrap();
    let batch = receiver.recv().await.expect("one merged archive batch");
    assert_eq!(
        batch.chunks,
        vec![
            Bytes::from_static(b"data: {\"delta\":\"first\"}\r\r"),
            Bytes::from_static(b"data: {\"delta\":\"second\"}\r\r"),
        ]
    );
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn deferred_crlf_continuation_and_following_event_share_one_archive_slot_in_order() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let mut deferred = DeferredResponseArchive::default();
    let first = [SseDeliveryFrame {
        bytes: Bytes::from_static(b"data: [DONE]\r\n\r"),
        billable: false,
    }];
    deferred.queue(&sender, &first, true, false).unwrap();
    assert!(receiver.try_recv().is_err());

    let second = [
        SseDeliveryFrame {
            bytes: Bytes::from_static(b"\n"),
            billable: false,
        },
        SseDeliveryFrame {
            bytes: Bytes::from_static(b": heartbeat\n\n"),
            billable: false,
        },
    ];
    deferred.queue(&sender, &second, false, true).unwrap();
    let batch = receiver.recv().await.expect("one merged archive batch");
    assert_eq!(
        batch.chunks,
        vec![
            Bytes::from_static(b"data: [DONE]\r\n\r\n"),
            Bytes::from_static(b": heartbeat\n\n"),
        ]
    );
    assert!(receiver.try_recv().is_err());
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
