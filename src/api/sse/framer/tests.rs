use super::*;

use crate::api::limits::{
    MAX_RESPONSES_SSE_EVENT_BYTES, MAX_SSE_FIELDS_PER_EVENT,
    MAX_SSE_FRAMED_BYTES_PER_NETWORK_CHUNK, MAX_SSE_FRAMES_PER_NETWORK_CHUNK,
    MAX_SSE_METADATA_ITEMS_PER_NETWORK_CHUNK,
};

fn event_with_field_count(fields: usize) -> Vec<u8> {
    let mut event = b"x\n".repeat(fields);
    event.push(b'\n');
    event
}

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
        // Standalone comments dispatch promptly; the following blank line is
        // a distinct empty control frame rather than a reason to delay it.
        assert_eq!(batch.events.len(), 2);
        let mut delivered = Vec::new();
        for event in batch.events {
            delivered.extend_from_slice(&event.bytes);
        }
        assert_eq!(delivered, heartbeat);
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

#[test]
fn accepts_exact_field_limit_and_rejects_one_extra_field() {
    let exact = event_with_field_count(MAX_SSE_FIELDS_PER_EVENT);
    let mut framer = BoundedSseFramer::default();
    let batch = framer.push(&exact);
    assert_eq!(batch.rejection, None);
    assert_eq!(batch.events.len(), 1);
    assert_eq!(batch.events[0].lines.len(), MAX_SSE_FIELDS_PER_EVENT);

    let overflow = event_with_field_count(MAX_SSE_FIELDS_PER_EVENT + 1);
    let mut framer = BoundedSseFramer::default();
    let batch = framer.push(&overflow);
    assert_eq!(batch.rejection, Some(SseFramerRejection::EventLimit));
    assert!(batch.events.is_empty());
}

#[test]
fn accepts_exact_metadata_budget_and_rejects_million_short_fields_within_2mib() {
    // Four events with 4,095 fields use exactly 16,384 retained field/event
    // metadata entries: 4 * (4,095 field lines + 1 event frame).
    let exact_event = event_with_field_count(MAX_SSE_FIELDS_PER_EVENT - 1);
    let exact = exact_event.repeat(4);
    assert_eq!(MAX_SSE_METADATA_ITEMS_PER_NETWORK_CHUNK, 4 * 4_096);
    let mut framer = BoundedSseFramer::default();
    let batch = framer.push(&exact);
    assert_eq!(batch.rejection, None);
    assert_eq!(batch.events.len(), 4);

    let mut plus_one = exact;
    plus_one.push(b'\n');
    let mut framer = BoundedSseFramer::default();
    let batch = framer.push(&plus_one);
    assert_eq!(batch.rejection, Some(SseFramerRejection::BatchLimit));
    assert_eq!(batch.events.len(), 4);

    let million_short_fields = event_with_field_count(MAX_SSE_FIELDS_PER_EVENT).repeat(255);
    assert!(million_short_fields.len() <= MAX_SSE_FRAMED_BYTES_PER_NETWORK_CHUNK);
    assert!(MAX_SSE_FIELDS_PER_EVENT * 255 > 1_000_000);
    let mut framer = BoundedSseFramer::default();
    let batch = framer.push(&million_short_fields);
    assert_eq!(batch.rejection, Some(SseFramerRejection::BatchLimit));
}

#[test]
fn emits_idle_comments_without_an_empty_line_and_keeps_pending_data_buffered() {
    let mut framer = BoundedSseFramer::default();
    let batch = framer.push(b": ping\n");
    assert_eq!(batch.events.len(), 1);
    assert_eq!(batch.events[0].bytes.as_ref(), b": ping\n");
    assert_eq!(batch.events[0].idle_control, Some(SseIdleControl::Comment));
    assert!(framer.is_complete());

    let batch = framer.push(b"id: resume\nretry: 1000\n");
    assert_eq!(batch.events.len(), 2);
    assert!(framer.is_complete());

    let mut framer = BoundedSseFramer::default();
    let batch = framer.push(b"data: pending\n: later\n");
    assert!(batch.events.is_empty());
    assert!(!framer.is_complete());
}
