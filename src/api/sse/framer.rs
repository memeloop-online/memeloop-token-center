use axum::body::Bytes;

use super::super::limits::{
    MAX_SSE_FIELDS_PER_EVENT, MAX_SSE_FRAMES_PER_NETWORK_CHUNK,
    MAX_SSE_METADATA_ITEMS_PER_NETWORK_CHUNK,
};
use crate::provider::SseFramingLimits;

fn reserve<T>(
    budget: &Option<std::sync::Arc<crate::gateway_body::memory::SseAllocationBudget>>,
    buffer: &mut Vec<T>,
    additional: usize,
) -> bool {
    budget
        .as_ref()
        .is_none_or(|budget| budget.reserve_vec(buffer, additional))
}

/// A bounded, stateful SSE framer. It preserves original wire bytes while
/// treating CR, LF, and CRLF as line endings. EOF never turns an unterminated
/// field block into an event.
pub(in crate::api) struct BoundedSseFramer {
    limits: SseFramingLimits,
    allocation_budget: Option<std::sync::Arc<crate::gateway_body::memory::SseAllocationBudget>>,
    event: Vec<u8>,
    line: Vec<u8>,
    lines: Vec<BoundedSseLine>,
    discarding_event: bool,
    discarding_line_has_data: bool,
    skip_lf_after_cr: bool,
    emit_lf_continuation: bool,
    emitted_cr_event_bytes: usize,
    completed_event: Option<PendingSseEvent>,
    batch_rejected: bool,
}

impl Default for BoundedSseFramer {
    fn default() -> Self {
        Self::with_limits(SseFramingLimits::default())
    }
}

pub(in crate::api) struct BoundedSseFrameBatch {
    pub(in crate::api) events: Vec<BoundedSseEvent>,
    pub(in crate::api) rejection: Option<SseFramerRejection>,
    framed_bytes: usize,
    metadata_items: usize,
    allocation_budget: Option<std::sync::Arc<crate::gateway_body::memory::SseAllocationBudget>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::api) enum SseFramerRejection {
    EventLimit,
    BatchLimit,
    MemoryCapacity,
}

impl SseFramerRejection {
    pub(in crate::api) fn error_code(self) -> &'static str {
        match self {
            Self::MemoryCapacity => "upstream_response_memory_capacity",
            Self::EventLimit => "upstream_response_event_too_large",
            Self::BatchLimit => "upstream_response_event_batch_too_large",
        }
    }
}

pub(in crate::api) struct BoundedSseEvent {
    pub(in crate::api) bytes: Bytes,
    pub(in crate::api) lines: Vec<BoundedSseLine>,
    pub(in crate::api) terminator: Vec<u8>,
    pub(in crate::api) is_line_ending_continuation: bool,
    pub(in crate::api) idle_control: Option<SseIdleControl>,
}

pub(in crate::api) struct BoundedSseLine {
    pub(in crate::api) value: Vec<u8>,
    pub(in crate::api) ending: Vec<u8>,
}

pub(in crate::api) const SAFE_SSE_HEARTBEAT_COMMENT: &[u8] = b": heartbeat";

#[derive(Clone, Copy, Eq, PartialEq)]
pub(in crate::api) enum SseEventMetadataPolicy {
    DataOnly,
    ValidatedEventNames,
}

/// Preserve data bytes and framing while allowing only a fixed heartbeat from
/// upstream metadata. Event names require an explicit validated policy; ids,
/// retry hints, and unknown fields are never copied to delivery or archive.
pub(in crate::api) fn redacted_sse_event_bytes(
    event: &BoundedSseEvent,
    metadata_policy: SseEventMetadataPolicy,
) -> Bytes {
    let mut output = Vec::with_capacity(event.bytes.len());
    for line in &event.lines {
        if line.value.starts_with(b":") {
            output.extend_from_slice(SAFE_SSE_HEARTBEAT_COMMENT);
        } else if is_sse_field_line(&line.value, b"data")
            || (metadata_policy == SseEventMetadataPolicy::ValidatedEventNames
                && is_sse_field_line(&line.value, b"event"))
        {
            output.extend_from_slice(&line.value);
        } else {
            continue;
        }
        output.extend_from_slice(&line.ending);
    }
    output.extend_from_slice(&event.terminator);
    Bytes::from(output)
}

struct PendingSseEvent {
    bytes: Vec<u8>,
    lines: Vec<BoundedSseLine>,
    terminator: Vec<u8>,
    crlf_target: CrLfTarget,
    is_line_ending_continuation: bool,
    idle_control: Option<SseIdleControl>,
}

#[derive(Clone, Copy)]
enum CrLfTarget {
    Terminator,
    LastLine,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::api) enum SseIdleControl {
    Comment,
    IdOrRetry,
}

impl BoundedSseFramer {
    pub(in crate::api) fn set_allocation_budget(
        &mut self,
        budget: std::sync::Arc<crate::gateway_body::memory::SseAllocationBudget>,
    ) {
        self.allocation_budget = Some(budget);
    }

    pub(in crate::api) fn retained_capacity(&self) -> usize {
        fn lines_capacity(lines: &Vec<BoundedSseLine>) -> usize {
            lines.capacity() * std::mem::size_of::<BoundedSseLine>()
                + lines
                    .iter()
                    .map(|line| line.value.capacity() + line.ending.capacity())
                    .sum::<usize>()
        }
        self.event.capacity()
            + self.line.capacity()
            + lines_capacity(&self.lines)
            + self.completed_event.as_ref().map_or(0, |event| {
                event.bytes.capacity() + event.terminator.capacity() + lines_capacity(&event.lines)
            })
    }

    pub(in crate::api) fn with_limits(limits: SseFramingLimits) -> Self {
        Self {
            limits,
            allocation_budget: None,
            event: Vec::new(),
            line: Vec::new(),
            lines: Vec::new(),
            discarding_event: false,
            discarding_line_has_data: false,
            skip_lf_after_cr: false,
            emit_lf_continuation: false,
            emitted_cr_event_bytes: 0,
            completed_event: None,
            batch_rejected: false,
        }
    }

    pub(in crate::api) fn push(&mut self, chunk: &[u8]) -> BoundedSseFrameBatch {
        let mut batch = BoundedSseFrameBatch {
            events: Vec::new(),
            rejection: None,
            framed_bytes: 0,
            metadata_items: 0,
            allocation_budget: self.allocation_budget.clone(),
        };
        if self.batch_rejected {
            batch.reject(SseFramerRejection::BatchLimit);
            return batch;
        }
        for &byte in chunk {
            if matches!(batch.rejection, Some(SseFramerRejection::MemoryCapacity)) {
                break;
            }
            // Once the per-network-chunk product ceiling is crossed, no
            // caller may safely use a partial prefix. Stop before allocating
            // or classifying the remaining tiny events in this raw chunk.
            if matches!(batch.rejection, Some(SseFramerRejection::BatchLimit)) {
                self.batch_rejected = true;
                break;
            }
            if let Some(mut completed) = self.completed_event.take() {
                if self.skip_lf_after_cr && byte == b'\n' {
                    if completed.bytes.len() >= self.limits.event_bytes {
                        self.skip_lf_after_cr = false;
                        self.emit_lf_continuation = false;
                        self.emitted_cr_event_bytes = 0;
                        batch.reject(SseFramerRejection::EventLimit);
                        continue;
                    }
                    if !reserve(&self.allocation_budget, &mut completed.bytes, 1)
                        || !reserve(&self.allocation_budget, &mut completed.terminator, 1)
                    {
                        batch.reject(SseFramerRejection::MemoryCapacity);
                        break;
                    }
                    completed.bytes.push(byte);
                    match completed.crlf_target {
                        CrLfTarget::Terminator => completed.terminator.push(byte),
                        CrLfTarget::LastLine => {
                            if let Some(line) = completed.lines.last_mut() {
                                if !reserve(&self.allocation_budget, &mut line.ending, 1) {
                                    batch.reject(SseFramerRejection::MemoryCapacity);
                                    break;
                                }
                                line.ending.push(byte);
                            }
                        }
                    }
                    self.skip_lf_after_cr = false;
                    self.emit_lf_continuation = false;
                    self.emitted_cr_event_bytes = 0;
                    Self::emit(&mut batch, completed, self.limits);
                    continue;
                }
                self.skip_lf_after_cr = false;
                self.emit_lf_continuation = false;
                self.emitted_cr_event_bytes = 0;
                Self::emit(&mut batch, completed, self.limits);
            }
            if self.skip_lf_after_cr {
                self.skip_lf_after_cr = false;
                if byte == b'\n' {
                    if self.discarding_event {
                        continue;
                    }
                    if let Some(line) = self.lines.last_mut() {
                        if self.event.len() >= self.limits.event_bytes {
                            self.discarding_event = true;
                            self.discarding_line_has_data = false;
                            self.event.clear();
                            self.line.clear();
                            self.lines.clear();
                            batch.reject(SseFramerRejection::EventLimit);
                            continue;
                        }
                        if !reserve(&self.allocation_budget, &mut line.ending, 1)
                            || !reserve(&self.allocation_budget, &mut self.event, 1)
                        {
                            batch.reject(SseFramerRejection::MemoryCapacity);
                            break;
                        }
                        line.ending.push(byte);
                        self.event.push(byte);
                    } else if self.emit_lf_continuation {
                        if self.emitted_cr_event_bytes >= self.limits.event_bytes {
                            batch.reject(SseFramerRejection::EventLimit);
                        } else {
                            if self
                                .allocation_budget
                                .as_ref()
                                .is_some_and(|budget| !budget.reserve_allocation(2))
                            {
                                batch.reject(SseFramerRejection::MemoryCapacity);
                                break;
                            }
                            Self::emit(
                                &mut batch,
                                PendingSseEvent {
                                    bytes: vec![b'\n'],
                                    lines: Vec::new(),
                                    terminator: vec![b'\n'],
                                    crlf_target: CrLfTarget::Terminator,
                                    is_line_ending_continuation: true,
                                    idle_control: None,
                                },
                                self.limits,
                            );
                        }
                    }
                    self.emit_lf_continuation = false;
                    self.emitted_cr_event_bytes = 0;
                    continue;
                }
                self.emitted_cr_event_bytes = 0;
            }
            if matches!(byte, b'\n' | b'\r')
                && self
                    .allocation_budget
                    .as_ref()
                    .is_some_and(|budget| !budget.reserve_allocation(1))
            {
                batch.reject(SseFramerRejection::MemoryCapacity);
                break;
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
            self.emitted_cr_event_bytes = completed.bytes.len();
            Self::emit(&mut batch, completed, self.limits);
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

    pub(in crate::api) fn allocation_budget(
        &self,
    ) -> Option<&std::sync::Arc<crate::gateway_body::memory::SseAllocationBudget>> {
        self.allocation_budget.as_ref()
    }

    pub(in crate::api) fn has_pending_crlf_continuation(&self) -> bool {
        self.skip_lf_after_cr
    }

    fn push_field_byte(&mut self, byte: u8, batch: &mut BoundedSseFrameBatch) {
        if self.discarding_event {
            self.discarding_line_has_data = true;
            return;
        }
        if self.event.len() >= self.limits.event_bytes {
            self.discarding_event = true;
            self.discarding_line_has_data = true;
            self.event.clear();
            self.line.clear();
            self.lines.clear();
            batch.reject(SseFramerRejection::EventLimit);
            return;
        }
        if !reserve(&self.allocation_budget, &mut self.event, 1)
            || !reserve(&self.allocation_budget, &mut self.line, 1)
        {
            batch.reject(SseFramerRejection::MemoryCapacity);
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
        if self.event.len().saturating_add(ending.len()) > self.limits.event_bytes {
            self.discarding_event = !self.line.is_empty();
            self.discarding_line_has_data = false;
            self.event.clear();
            self.line.clear();
            self.lines.clear();
            batch.reject(SseFramerRejection::EventLimit);
            return;
        }
        if !reserve(&self.allocation_budget, &mut self.event, ending.len()) {
            batch.reject(SseFramerRejection::MemoryCapacity);
            return;
        }
        self.event.extend_from_slice(&ending);
        if self.line.is_empty() {
            let completed = PendingSseEvent {
                bytes: std::mem::take(&mut self.event),
                lines: std::mem::take(&mut self.lines),
                terminator: ending,
                crlf_target: CrLfTarget::Terminator,
                is_line_ending_continuation: false,
                idle_control: None,
            };
            if completed.terminator.first() == Some(&b'\r') {
                self.completed_event = Some(completed);
            } else {
                Self::emit(batch, completed, self.limits);
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
        let line = std::mem::take(&mut self.line);
        if self.lines.is_empty()
            && let Some(idle_control) = idle_control_kind(&line)
        {
            if !batch.reserve_metadata_item() {
                self.event.clear();
                self.line.clear();
                self.lines.clear();
                return;
            }
            if self.allocation_budget.as_ref().is_some_and(|budget| {
                !budget.reserve_allocation(std::mem::size_of::<BoundedSseLine>())
            }) {
                batch.reject(SseFramerRejection::MemoryCapacity);
                return;
            }
            let completed = PendingSseEvent {
                bytes: std::mem::take(&mut self.event),
                lines: vec![BoundedSseLine {
                    value: line,
                    ending,
                }],
                terminator: Vec::new(),
                crlf_target: CrLfTarget::LastLine,
                is_line_ending_continuation: false,
                idle_control: Some(idle_control),
            };
            if completed.lines[0].ending.first() == Some(&b'\r') {
                self.completed_event = Some(completed);
            } else {
                Self::emit(batch, completed, self.limits);
            }
            return;
        }
        if !batch.reserve_metadata_item() {
            self.event.clear();
            self.line.clear();
            self.lines.clear();
            return;
        }
        if !reserve(&self.allocation_budget, &mut self.lines, 1) {
            batch.reject(SseFramerRejection::MemoryCapacity);
            return;
        }
        self.lines.push(BoundedSseLine {
            value: line,
            ending,
        });
    }

    fn emit(batch: &mut BoundedSseFrameBatch, pending: PendingSseEvent, limits: SseFramingLimits) {
        if batch.events.len() >= MAX_SSE_FRAMES_PER_NETWORK_CHUNK
            || batch.framed_bytes.saturating_add(pending.bytes.len()) > limits.framed_bytes
        {
            batch.reject(SseFramerRejection::BatchLimit);
            return;
        }
        if !batch.reserve_metadata_item() {
            return;
        }
        batch.framed_bytes = batch.framed_bytes.saturating_add(pending.bytes.len());
        if !reserve(&batch.allocation_budget, &mut batch.events, 1) {
            batch.reject(SseFramerRejection::MemoryCapacity);
            return;
        }
        batch.events.push(BoundedSseEvent {
            bytes: Bytes::from(pending.bytes),
            lines: pending.lines,
            terminator: pending.terminator,
            is_line_ending_continuation: pending.is_line_ending_continuation,
            idle_control: pending.idle_control,
        });
    }
}

impl BoundedSseEvent {
    pub(in crate::api) fn reserve_processing(
        &self,
        budget: &crate::gateway_body::memory::SseAllocationBudget,
    ) -> bool {
        let mut scanner = crate::gateway_body::memory::JsonMemoryScanner::default();
        let mut nodes = 0;
        for line in &self.lines {
            nodes = scanner.observe(&line.value);
        }
        // Joined data, parsed strings, serialized data, sanitized output,
        // capture output and terminal reconstruction coexist during conversion.
        self.bytes
            .len()
            .checked_mul(6)
            .and_then(|bytes| {
                nodes
                    .checked_mul(256)
                    .and_then(|nodes| bytes.checked_add(nodes))
            })
            .is_some_and(|bytes| budget.reserve_allocation(bytes))
    }
}

impl BoundedSseFrameBatch {
    fn reserve_metadata_item(&mut self) -> bool {
        if self.metadata_items >= MAX_SSE_METADATA_ITEMS_PER_NETWORK_CHUNK {
            self.reject(SseFramerRejection::BatchLimit);
            return false;
        }
        self.metadata_items += 1;
        true
    }

    fn reject(&mut self, rejection: SseFramerRejection) {
        if matches!(rejection, SseFramerRejection::BatchLimit) || self.rejection.is_none() {
            self.rejection = Some(rejection);
        }
    }
}

fn idle_control_kind(line: &[u8]) -> Option<SseIdleControl> {
    if line.starts_with(b":") {
        Some(SseIdleControl::Comment)
    } else if is_sse_field_line(line, b"id") || is_sse_field_line(line, b"retry") {
        Some(SseIdleControl::IdOrRetry)
    } else {
        None
    }
}

fn is_sse_field_line(line: &[u8], field: &[u8]) -> bool {
    line == field
        || line
            .strip_prefix(field)
            .is_some_and(|rest| rest.starts_with(b":"))
}

#[cfg(test)]
#[path = "framer/tests.rs"]
mod tests;
