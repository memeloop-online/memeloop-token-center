use axum::body::Bytes;

use super::super::limits::{
    MAX_RESPONSES_SSE_EVENT_BYTES, MAX_SSE_FIELDS_PER_EVENT,
    MAX_SSE_FRAMED_BYTES_PER_NETWORK_CHUNK, MAX_SSE_FRAMES_PER_NETWORK_CHUNK,
};

/// A bounded, stateful SSE framer. It preserves original wire bytes while
/// treating CR, LF, and CRLF as line endings. EOF never turns an unterminated
/// field block into an event.
#[derive(Default)]
pub(in crate::api) struct BoundedSseFramer {
    event: Vec<u8>,
    line: Vec<u8>,
    lines: Vec<BoundedSseLine>,
    discarding_event: bool,
    discarding_line_has_data: bool,
    skip_lf_after_cr: bool,
    emit_lf_continuation: bool,
    completed_event: Option<PendingSseEvent>,
    batch_rejected: bool,
}

pub(in crate::api) struct BoundedSseFrameBatch {
    pub(in crate::api) events: Vec<BoundedSseEvent>,
    pub(in crate::api) rejection: Option<SseFramerRejection>,
    framed_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::api) enum SseFramerRejection {
    EventLimit,
    BatchLimit,
}

pub(in crate::api) struct BoundedSseEvent {
    pub(in crate::api) bytes: Bytes,
    pub(in crate::api) lines: Vec<BoundedSseLine>,
    pub(in crate::api) terminator: Vec<u8>,
}

pub(in crate::api) struct BoundedSseLine {
    pub(in crate::api) value: Vec<u8>,
    pub(in crate::api) ending: Vec<u8>,
}

struct PendingSseEvent {
    bytes: Vec<u8>,
    lines: Vec<BoundedSseLine>,
    terminator: Vec<u8>,
}

impl BoundedSseFramer {
    pub(in crate::api) fn push(&mut self, chunk: &[u8]) -> BoundedSseFrameBatch {
        let mut batch = BoundedSseFrameBatch {
            events: Vec::new(),
            rejection: None,
            framed_bytes: 0,
        };
        if self.batch_rejected {
            batch.reject(SseFramerRejection::BatchLimit);
            return batch;
        }
        for &byte in chunk {
            // Once the per-network-chunk product ceiling is crossed, no
            // caller may safely use a partial prefix. Stop before allocating
            // or classifying the remaining tiny events in this raw chunk.
            if matches!(batch.rejection, Some(SseFramerRejection::BatchLimit)) {
                self.batch_rejected = true;
                break;
            }
            if let Some(mut completed) = self.completed_event.take() {
                if self.skip_lf_after_cr && byte == b'\n' {
                    completed.bytes.push(byte);
                    completed.terminator.push(byte);
                    self.skip_lf_after_cr = false;
                    self.emit_lf_continuation = false;
                    Self::emit(&mut batch, completed);
                    continue;
                }
                self.skip_lf_after_cr = false;
                self.emit_lf_continuation = false;
                Self::emit(&mut batch, completed);
            }
            if self.skip_lf_after_cr {
                self.skip_lf_after_cr = false;
                if byte == b'\n' {
                    if self.discarding_event {
                        continue;
                    }
                    if let Some(line) = self.lines.last_mut() {
                        if self.event.len() >= MAX_RESPONSES_SSE_EVENT_BYTES {
                            self.discarding_event = true;
                            self.discarding_line_has_data = false;
                            self.event.clear();
                            self.line.clear();
                            self.lines.clear();
                            batch.reject(SseFramerRejection::EventLimit);
                            continue;
                        }
                        line.ending.push(byte);
                        self.event.push(byte);
                    } else if self.emit_lf_continuation {
                        Self::emit(
                            &mut batch,
                            PendingSseEvent {
                                bytes: vec![b'\n'],
                                lines: Vec::new(),
                                terminator: vec![b'\n'],
                            },
                        );
                    }
                    self.emit_lf_continuation = false;
                    continue;
                }
            }
            match byte {
                b'\n' => self.finish_line(vec![byte], &mut batch),
                b'\r' => {
                    self.finish_line(vec![byte], &mut batch);
                    self.skip_lf_after_cr = true;
                }
                _ => self.push_field_byte(byte, &mut batch),
            }
        }
        if let Some(completed) = self.completed_event.take() {
            Self::emit(&mut batch, completed);
            self.emit_lf_continuation = true;
        }
        if matches!(batch.rejection, Some(SseFramerRejection::BatchLimit)) {
            self.batch_rejected = true;
        }
        batch
    }

    pub(in crate::api) fn is_complete(&self) -> bool {
        !self.discarding_event && self.event.is_empty() && self.line.is_empty()
    }

    fn push_field_byte(&mut self, byte: u8, batch: &mut BoundedSseFrameBatch) {
        if self.discarding_event {
            self.discarding_line_has_data = true;
            return;
        }
        if self.event.len() >= MAX_RESPONSES_SSE_EVENT_BYTES {
            self.discarding_event = true;
            self.discarding_line_has_data = true;
            self.event.clear();
            self.line.clear();
            self.lines.clear();
            batch.reject(SseFramerRejection::EventLimit);
            return;
        }
        self.event.push(byte);
        self.line.push(byte);
    }

    fn finish_line(&mut self, ending: Vec<u8>, batch: &mut BoundedSseFrameBatch) {
        if self.discarding_event {
            if !self.discarding_line_has_data {
                self.discarding_event = false;
            }
            self.discarding_line_has_data = false;
            return;
        }
        if self.event.len().saturating_add(ending.len()) > MAX_RESPONSES_SSE_EVENT_BYTES {
            self.discarding_event = !self.line.is_empty();
            self.discarding_line_has_data = false;
            self.event.clear();
            self.line.clear();
            self.lines.clear();
            batch.reject(SseFramerRejection::EventLimit);
            return;
        }
        self.event.extend_from_slice(&ending);
        if self.line.is_empty() {
            let completed = PendingSseEvent {
                bytes: std::mem::take(&mut self.event),
                lines: std::mem::take(&mut self.lines),
                terminator: ending,
            };
            if completed.terminator.first() == Some(&b'\r') {
                self.completed_event = Some(completed);
            } else {
                Self::emit(batch, completed);
            }
            return;
        }
        if self.lines.len() >= MAX_SSE_FIELDS_PER_EVENT {
            self.discarding_event = true;
            self.discarding_line_has_data = false;
            self.event.clear();
            self.line.clear();
            self.lines.clear();
            batch.reject(SseFramerRejection::EventLimit);
            return;
        }
        self.lines.push(BoundedSseLine {
            value: std::mem::take(&mut self.line),
            ending,
        });
    }

    fn emit(batch: &mut BoundedSseFrameBatch, pending: PendingSseEvent) {
        if batch.events.len() >= MAX_SSE_FRAMES_PER_NETWORK_CHUNK
            || batch.framed_bytes.saturating_add(pending.bytes.len())
                > MAX_SSE_FRAMED_BYTES_PER_NETWORK_CHUNK
        {
            batch.reject(SseFramerRejection::BatchLimit);
            return;
        }
        batch.framed_bytes = batch.framed_bytes.saturating_add(pending.bytes.len());
        batch.events.push(BoundedSseEvent {
            bytes: Bytes::from(pending.bytes),
            lines: pending.lines,
            terminator: pending.terminator,
        });
    }
}

impl BoundedSseFrameBatch {
    fn reject(&mut self, rejection: SseFramerRejection) {
        if matches!(rejection, SseFramerRejection::BatchLimit) || self.rejection.is_none() {
            self.rejection = Some(rejection);
        }
    }
}

#[cfg(test)]
#[path = "framer/tests.rs"]
mod tests;
