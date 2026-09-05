use super::*;

use crate::api::limits::{
    MAX_RESPONSES_SSE_EVENT_BYTES, MAX_SSE_FRAMED_BYTES_PER_NETWORK_CHUNK,
    MAX_SSE_FRAMES_PER_NETWORK_CHUNK,
};

#[test]
fn accepts_cr_lf_and_crlf_and_never_flushes_eof_data() {
    for heartbeat in [
        b": heartbeat\n\n".as_slice(),
        b": heartbeat\r\r".as_slice(),
        b": heartbeat\r\n\r\n".as_slice(),
    ] {
        let mut framer = BoundedSseFramer::default();
        let batch = framer.push(heartbeat);
        assert_eq!(batch.rejection, None);
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0].bytes.as_ref(), heartbeat);
        assert!(framer.is_complete());
    }

    let mut framer = BoundedSseFramer::default();
    let batch = framer.push(b"data: {\"type\":\"response.created\"}");
    assert!(batch.events.is_empty());
    assert!(!framer.is_complete());
}

#[test]
fn rejects_large_small_event_batches_before_materializing_them() {
    let event = b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-small\"}}\n\n";
    let chunk = event.repeat(MAX_SSE_FRAMES_PER_NETWORK_CHUNK + 1);
    assert!(chunk.len() < MAX_SSE_FRAMED_BYTES_PER_NETWORK_CHUNK);
    let mut framer = BoundedSseFramer::default();
    let batch = framer.push(&chunk);
    assert_eq!(batch.rejection, Some(SseFramerRejection::BatchLimit));
    assert_eq!(batch.events.len(), MAX_SSE_FRAMES_PER_NETWORK_CHUNK);
}

#[test]
fn rejects_a_chunk_whose_framed_bytes_exceed_the_batch_budget() {
    let payload = vec![b'x'; MAX_RESPONSES_SSE_EVENT_BYTES - b"data: ".len() - 2];
    let mut event = b"data: ".to_vec();
    event.extend_from_slice(&payload);
    event.extend_from_slice(b"\n\n");
    let chunk = event.repeat(MAX_SSE_FRAMED_BYTES_PER_NETWORK_CHUNK / event.len() + 1);
    assert!(chunk.len() > MAX_SSE_FRAMED_BYTES_PER_NETWORK_CHUNK);
    assert!(chunk.len() / event.len() < MAX_SSE_FRAMES_PER_NETWORK_CHUNK);
    let mut framer = BoundedSseFramer::default();
    let batch = framer.push(&chunk);
    assert_eq!(batch.rejection, Some(SseFramerRejection::BatchLimit));
}
