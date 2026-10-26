use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::task::Poll;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

// Charge raw bytes, bounded JSON trees/clones and encrypted batch copies before
// retaining input. This is a process-wide admission budget, not a metric.
pub(crate) const REQUEST_MEMORY_WEIGHT: usize = 3;
pub(crate) const CAPTURE_MEMORY_WEIGHT: usize = 3;
pub(crate) const MAX_BUFFERED_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const SSE_STREAMING_MEMORY_WEIGHT: usize = 2;
const UNIT_BYTES: usize = 64 * 1024;

#[derive(Default)]
pub(crate) struct JsonMemoryScanner {
    quoted: bool,
    escaped: bool,
    nodes: usize,
}

impl JsonMemoryScanner {
    pub(crate) fn observe(&mut self, body: &[u8]) -> usize {
        for byte in body {
            if self.quoted {
                if self.escaped {
                    self.escaped = false;
                } else if *byte == b'\\' {
                    self.escaped = true;
                } else if *byte == b'"' {
                    self.quoted = false;
                }
            } else if *byte == b'"' {
                self.quoted = true;
                self.nodes = self.nodes.saturating_add(1);
            } else if matches!(*byte, b'[' | b'{' | b',' | b':') {
                self.nodes = self.nodes.saturating_add(1);
            }
        }
        self.nodes.saturating_add(1)
    }
}

pub(crate) struct ProxyMemoryBudget {
    capacity: usize,
    permits: Arc<Semaphore>,
    retained_requests: Arc<Semaphore>,
}

impl ProxyMemoryBudget {
    pub(crate) fn new(bytes: u32) -> Self {
        Self {
            capacity: bytes as usize,
            permits: Arc::new(Semaphore::new(bytes as usize / UNIT_BYTES)),
            retained_requests: Arc::new(Semaphore::new(bytes as usize / 4 / UNIT_BYTES)),
        }
    }

    pub(crate) fn reservation(&self) -> Arc<ProxyMemoryReservation> {
        Arc::new(ProxyMemoryReservation {
            permits: self.permits.clone(),
            held: Mutex::new((0, None)),
            response_reserved: AtomicBool::new(false),
            response_bytes: AtomicUsize::new(0),
            retained_requests: self.retained_requests.clone(),
            retained: Mutex::new(None),
            json_body_ceiling: AtomicUsize::new(0),
            json_node_ceiling: AtomicUsize::new(0),
        })
    }

    pub(crate) fn snapshot(&self) -> (usize, usize, usize, usize) {
        let lifecycle_limit = self.capacity / UNIT_BYTES * UNIT_BYTES;
        let retained_limit = self.capacity / 4 / UNIT_BYTES * UNIT_BYTES;
        (
            lifecycle_limit.saturating_sub(self.permits.available_permits() * UNIT_BYTES),
            lifecycle_limit,
            retained_limit.saturating_sub(self.retained_requests.available_permits() * UNIT_BYTES),
            retained_limit,
        )
    }

    pub(crate) fn temporary(
        &self,
        bytes: usize,
    ) -> Result<Arc<ProxyMemoryReservation>, crate::error::AppError> {
        let reservation = self.reservation();
        if !reservation.try_grow(bytes, 1) {
            return Err(crate::error::AppError::capacity(
                "proxy_memory_temporary",
                crate::error::ErrorFacts::new().bytes(bytes as u64),
            ));
        }
        Ok(reservation)
    }
}

pub(crate) struct ProxyMemoryReservation {
    permits: Arc<Semaphore>,
    held: Mutex<(usize, Option<OwnedSemaphorePermit>)>,
    response_reserved: AtomicBool,
    response_bytes: AtomicUsize,
    retained_requests: Arc<Semaphore>,
    retained: Mutex<Option<OwnedSemaphorePermit>>,
    json_body_ceiling: AtomicUsize,
    json_node_ceiling: AtomicUsize,
}

pub(crate) struct ConversationProjectionPermit {
    _permit: OwnedSemaphorePermit,
}

pub(crate) struct ArchiveOutputMemory {
    reservation: Arc<ProxyMemoryReservation>,
    bytes: usize,
}

pub(crate) struct SseStreamingMemory {
    reservation: Arc<ProxyMemoryReservation>,
    bytes: usize,
}

impl Drop for ArchiveOutputMemory {
    fn drop(&mut self) {
        self.reservation.release(self.bytes, 1);
    }
}

impl Drop for SseStreamingMemory {
    fn drop(&mut self) {
        self.reservation.release(self.bytes, 1);
    }
}

impl ProxyMemoryReservation {
    pub(crate) fn try_reserve_ingress_bytes(&self, bytes: usize) -> Option<OwnedSemaphorePermit> {
        let units = u32::try_from(bytes.div_ceil(UNIT_BYTES)).ok()?;
        self.permits.clone().try_acquire_many_owned(units).ok()
    }

    /// Reserve one request-local envelope for the two independently retained
    /// SSE maxima: framing/sanitizer state and terminal delivery state. This turns the
    /// configured per-stream byte ceiling into process-wide concurrency
    /// admission instead of allowing every active stream to allocate its
    /// maximum independently of the global proxy memory budget.
    pub(crate) fn try_reserve_sse_streaming(
        self: &Arc<Self>,
        framed_bytes: usize,
    ) -> Option<SseStreamingMemory> {
        let bytes = framed_bytes.checked_mul(SSE_STREAMING_MEMORY_WEIGHT)?;
        self.try_grow(bytes, 1).then(|| SseStreamingMemory {
            reservation: self.clone(),
            bytes,
        })
    }

    pub(crate) fn try_reserve_archive_output(
        self: &Arc<Self>,
        bytes: usize,
    ) -> Option<ArchiveOutputMemory> {
        let bytes = bytes.max(256);
        self.try_grow(bytes, 1).then(|| ArchiveOutputMemory {
            reservation: self.clone(),
            bytes,
        })
    }

    pub(crate) fn try_reserve_archive_json_transform(
        self: &Arc<Self>,
        body: &[u8],
    ) -> Option<ArchiveOutputMemory> {
        let nodes = JsonMemoryScanner::default().observe(body);
        // SSE retention simultaneously owns bounded framing bytes/lines,
        // joined data, a parsed Value, serialized retained data, and the
        // reconstructed event. Charge all six byte-sized copies plus tree
        // nodes before parsing; the producer queue acquires its own permit
        // when the final Bytes is handed off.
        let bytes = body
            .len()
            .checked_mul(6)?
            .checked_add(nodes.checked_mul(256)?)?;
        self.try_reserve_archive_output(bytes)
    }

    /// Memory is only an accelerator. Try once and return `None` when the
    /// permits are not available right now. Callers must spill or fail; they
    /// must not queue.
    fn acquire(semaphore: &Arc<Semaphore>, units: u32) -> Option<OwnedSemaphorePermit> {
        semaphore.clone().try_acquire_many_owned(units).ok()
    }

    pub(crate) fn has_buffered_response(&self) -> bool {
        self.response_reserved.load(Ordering::Acquire)
    }

    pub(crate) fn response_capture_fits(&self, bytes: usize) -> bool {
        bytes.saturating_mul(CAPTURE_MEMORY_WEIGHT) <= self.response_bytes.load(Ordering::Acquire)
    }

    pub(crate) fn finalize_request(&self, _deadline: tokio::time::Instant) -> bool {
        let units = {
            let Ok(held) = self.held.lock() else {
                return false;
            };
            let Ok(units) = u32::try_from(held.0.div_ceil(UNIT_BYTES)) else {
                return false;
            };
            units
        };
        let Some(permit) = Self::acquire(&self.retained_requests, units) else {
            return false;
        };
        let Ok(mut retained) = self.retained.lock() else {
            return false;
        };
        *retained = Some(permit);
        true
    }

    /// An actual SSE response does not allocate a buffered response body. It
    /// remains charged to the process-wide lifecycle budget, but must stop
    /// occupying the retained-request partition reserved for paths that can
    /// still need a maximum buffered response.
    pub(crate) fn release_retained_request_for_stream(&self) {
        if let Ok(mut retained) = self.retained.lock() {
            *retained = None;
        }
    }

    /// A requested native stream stays outside the retained partition until
    /// response headers prove that it must buffer after all. Claim its raw
    /// body plus terminal projection footprint without queuing. Losers fail
    /// immediately and release their raw bytes so a later try can reserve the
    /// buffered response.
    pub(crate) async fn reserve_unexpected_buffered_request(
        &self,
        projection_bytes: usize,
        projection_weight: usize,
    ) -> bool {
        let units = {
            let Ok(held) = self.held.lock() else {
                return false;
            };
            let Some(bytes) = projection_bytes
                .checked_mul(projection_weight)
                .and_then(|projection| held.0.checked_add(projection))
            else {
                return false;
            };
            let Ok(units) = u32::try_from(bytes.div_ceil(UNIT_BYTES)) else {
                return false;
            };
            units
        };
        let mut acquire = Box::pin(self.retained_requests.clone().acquire_many_owned(units));
        let Some(Ok(permit)) = std::future::poll_fn(|context| {
            Poll::Ready(match acquire.as_mut().poll(context) {
                Poll::Ready(result) => Some(result),
                Poll::Pending => None,
            })
        })
        .await
        else {
            return false;
        };
        let Ok(mut retained) = self.retained.lock() else {
            return false;
        };
        if retained.is_some() {
            return true;
        }
        *retained = Some(permit);
        true
    }

    pub(crate) fn reserve_buffered_response(
        &self,
        maximum: usize,
        _deadline: tokio::time::Instant,
    ) -> bool {
        if self.has_buffered_response() {
            return true;
        }
        if maximum > MAX_BUFFERED_RESPONSE_BYTES {
            return false;
        }
        let bytes = maximum
            .saturating_mul(CAPTURE_MEMORY_WEIGHT)
            .max(UNIT_BYTES);
        let units = bytes.div_ceil(UNIT_BYTES) as u32;
        let Some(permit) = Self::acquire(&self.permits, units) else {
            return false;
        };
        let Ok(mut held) = self.held.lock() else {
            return false;
        };
        held.0 += units as usize * UNIT_BYTES;
        match held.1.as_mut() {
            Some(existing) => existing.merge(permit),
            None => held.1 = Some(permit),
        }
        self.response_reserved.store(true, Ordering::Release);
        self.response_bytes
            .store(units as usize * UNIT_BYTES, Ordering::Release);
        true
    }

    /// Admit a buffered response envelope and its terminal conversation
    /// projection as one try. A miss returns immediately so the caller can
    /// fail the already-executed response instead of queueing.
    pub(crate) fn reserve_buffered_response_with_projection(
        &self,
        maximum: usize,
        projection_bytes: usize,
        projection_weight: usize,
        _deadline: tokio::time::Instant,
    ) -> Option<ConversationProjectionPermit> {
        if self.has_buffered_response() || maximum > MAX_BUFFERED_RESPONSE_BYTES {
            return None;
        }
        let response_bytes = maximum
            .saturating_mul(CAPTURE_MEMORY_WEIGHT)
            .max(UNIT_BYTES);
        let response_units = u32::try_from(response_bytes.div_ceil(UNIT_BYTES)).ok()?;
        let projection_bytes = projection_bytes.checked_mul(projection_weight)?;
        let projection_units = u32::try_from(projection_bytes.div_ceil(UNIT_BYTES)).ok()?;
        let total_units = response_units.checked_add(projection_units)?;
        let mut permit = Self::acquire(&self.permits, total_units)?;
        let projection = permit.split(projection_units as usize)?;
        let Ok(mut held) = self.held.lock() else {
            return None;
        };
        held.0 = held.0.checked_add(response_units as usize * UNIT_BYTES)?;
        match held.1.as_mut() {
            Some(existing) => existing.merge(permit),
            None => held.1 = Some(permit),
        }
        self.response_reserved.store(true, Ordering::Release);
        self.response_bytes
            .store(response_units as usize * UNIT_BYTES, Ordering::Release);
        Some(ConversationProjectionPermit {
            _permit: projection,
        })
    }

    pub(crate) fn response_json_fits(&self, bytes: &[u8]) -> bool {
        bounded_json_fits(bytes, self.response_bytes.load(Ordering::Acquire))
    }

    pub(crate) fn release(&self, bytes: usize, weight: usize) {
        let Ok(mut held) = self.held.lock() else {
            return;
        };
        let total = held.0.saturating_sub(bytes.saturating_mul(weight));
        let units = held.0.div_ceil(UNIT_BYTES) - total.div_ceil(UNIT_BYTES);
        if let Some(permit) = held.1.as_mut() {
            drop(permit.split(units));
        }
        held.0 = total;
        if let Ok(mut retained) = self.retained.lock()
            && let Some(permit) = retained.as_mut()
        {
            drop(permit.split(units.min(permit.num_permits())));
        }
    }

    /// Release every admission permit after all request-side owners have been
    /// dropped and the request has been rejected before response buffering.
    /// Terminal database settlement must not keep those permits, or a later
    /// admission cannot use the capacity.
    pub(crate) fn release_rejected_request(&self) {
        if let Ok(mut held) = self.held.lock() {
            *held = (0, None);
        }
        if let Ok(mut retained) = self.retained.lock() {
            *retained = None;
        }
        self.response_reserved.store(false, Ordering::Release);
        self.response_bytes.store(0, Ordering::Release);
        self.json_body_ceiling.store(0, Ordering::Release);
        self.json_node_ceiling.store(0, Ordering::Release);
    }

    /// Keep one raw request-body copy charged after the routed request JSON and
    /// provider working buffers have both been dropped. Terminal projection
    /// reacquires the two transient copies before reparsing the retained body.
    pub(crate) fn release_conversation_working_copies(&self, bytes: usize) {
        self.release(bytes, REQUEST_MEMORY_WEIGHT.saturating_sub(1));
    }

    /// A durable request spool can replace the raw in-memory conversation copy
    /// after routing. Unlike the in-memory path, no byte-sized reservation
    /// remains live while the upstream response is in flight.
    pub(crate) fn release_source_backed_conversation_working_copies(&self, bytes: usize) {
        self.release(bytes, REQUEST_MEMORY_WEIGHT);
    }

    /// Reserve the transient JSON-tree copies required to project a retained
    /// raw conversation body. The returned permit is independent of the
    /// long-lived request reservation and never occupies the buffered
    /// retained-request partition. A miss returns immediately.
    pub(crate) fn reserve_conversation_projection(
        &self,
        bytes: usize,
        weight: usize,
        _deadline: tokio::time::Instant,
    ) -> Option<ConversationProjectionPermit> {
        let weighted = bytes.checked_mul(weight)?;
        let units = u32::try_from(weighted.div_ceil(UNIT_BYTES)).ok()?;
        Self::acquire(&self.permits, units)
            .map(|permit| ConversationProjectionPermit { _permit: permit })
    }

    pub(crate) fn try_reserve_json(&self, body: &[u8]) -> bool {
        // Preflight without allocating a JSON tree. Charge string copies by
        // input bytes and tree/map nodes separately, so tiny-token arrays and
        // objects cannot evade the memory contract through expansion.
        let nodes = JsonMemoryScanner::default().observe(body);
        if !self.try_grow(body.len(), 3) || !self.try_grow(nodes, 256) {
            return false;
        }
        self.json_body_ceiling.store(body.len(), Ordering::Release);
        self.json_node_ceiling.store(nodes, Ordering::Release);
        true
    }

    pub(crate) fn try_reserve_rewrite(&self, body: &[u8]) -> bool {
        // The prepaid three-copy envelope already covers the current payload.
        // Grow only its high-water mark before parsing a replacement; charging
        // another entire envelope would reject unchanged maximum-size hooks.
        let nodes = JsonMemoryScanner::default().observe(body);
        let old_bytes = self.json_body_ceiling.load(Ordering::Acquire);
        let old_nodes = self.json_node_ceiling.load(Ordering::Acquire);
        if !self.try_grow(body.len().saturating_sub(old_bytes), 3)
            || !self.try_grow(nodes.saturating_sub(old_nodes), 256)
        {
            return false;
        }
        self.json_body_ceiling
            .fetch_max(body.len(), Ordering::AcqRel);
        self.json_node_ceiling.fetch_max(nodes, Ordering::AcqRel);
        true
    }

    pub(crate) fn try_grow(&self, bytes: usize, weight: usize) -> bool {
        let Some(weighted) = bytes.checked_mul(weight) else {
            return false;
        };
        let Ok(mut held) = self.held.lock() else {
            return false;
        };
        let Some(total) = held.0.checked_add(weighted) else {
            return false;
        };
        let Ok(units) = u32::try_from(
            total
                .div_ceil(UNIT_BYTES)
                .saturating_sub(held.0.div_ceil(UNIT_BYTES)),
        ) else {
            return false;
        };
        let Ok(permit) = self.permits.clone().try_acquire_many_owned(units) else {
            return false;
        };
        match held.1.as_mut() {
            Some(existing) => existing.merge(permit),
            None => held.1 = Some(permit),
        }
        held.0 = total;
        true
    }
}

pub(crate) fn bounded_json_fits(bytes: &[u8], budget: usize) -> bool {
    let nodes = JsonMemoryScanner::default().observe(bytes);
    bytes
        .len()
        .saturating_mul(2)
        .saturating_add(nodes.saturating_mul(256))
        <= budget
}

pub(crate) fn json_encoded_length(
    value: &serde_json::Value,
) -> Result<usize, crate::error::AppError> {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .ok_or_else(|| std::io::Error::other("JSON size overflow"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, value).map_err(|_| crate::error::AppError::internal("memory_flush_1"))?;
    Ok(count.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unaligned_budget_snapshot_reports_actual_units_and_returns_to_zero() {
        let budget = ProxyMemoryBudget::new(256 * 1024 * 1024 + 1);
        assert_eq!(
            budget.snapshot(),
            (0, 256 * 1024 * 1024, 0, 64 * 1024 * 1024)
        );
        let owner = budget.reservation();
        assert!(owner.try_grow(1, 1));
        assert_eq!(budget.snapshot().0, UNIT_BYTES);
        drop(owner);
        assert_eq!(budget.snapshot().0, 0);
    }

    #[test]
    fn json_scanner_ignores_code_punctuation_and_preserves_escape_state_across_chunks() {
        let body = br#"{"text":"[{},:] \"quoted\"", "values":[0,1]}"#;
        let whole = JsonMemoryScanner::default().observe(body);
        for split in 0..body.len() {
            let mut scanner = JsonMemoryScanner::default();
            scanner.observe(&body[..split]);
            assert_eq!(scanner.observe(&body[split..]), whole);
        }
        assert!(whole < 16);
    }

    #[test]
    fn weighted_budget_rejects_before_retention_and_releases_only_last_owner() {
        let budget = ProxyMemoryBudget::new(128 * 1024 * 1024);
        let first = budget.reservation();
        assert!(first.try_grow(128 * 1024 * 1024, 1));
        let background_owner = first.clone();
        let second = budget.reservation();
        assert!(!second.try_grow(1, REQUEST_MEMORY_WEIGHT));
        drop(first);
        assert!(!second.try_grow(1, REQUEST_MEMORY_WEIGHT));
        drop(background_owner);
        assert!(second.try_grow(128 * 1024 * 1024, 1));
    }

    #[test]
    fn oversized_and_overflow_requests_cannot_consume_permits() {
        let budget = ProxyMemoryBudget::new(128 * 1024 * 1024);
        let request = budget.reservation();
        assert!(!request.try_grow(64 * 1024 * 1024, REQUEST_MEMORY_WEIGHT));
        assert!(!request.try_grow(usize::MAX, REQUEST_MEMORY_WEIGHT));
        assert!(request.try_grow(1024, REQUEST_MEMORY_WEIGHT));
    }

    #[test]
    fn archive_json_transform_is_hard_bounded_and_releases_its_permit() {
        let budget = ProxyMemoryBudget::new(256 * 1024);
        let blocker = budget.reservation();
        assert!(blocker.try_grow(256 * 1024, 1));
        let archive = budget.reservation();
        assert!(
            archive
                .try_reserve_archive_json_transform(br#"{"data":"data:image/x,"}"#)
                .is_none()
        );
        drop(blocker);
        let permit = archive
            .try_reserve_archive_json_transform(br#"{"data":"data:image/x,"}"#)
            .expect("archive transform fits after capacity is released");
        assert!(budget.snapshot().0 > 0);
        drop(permit);
        drop(archive);
        assert_eq!(budget.snapshot().0, 0);

        let large_frame = vec![b'x'; 64 * 1024];
        let insufficient = ProxyMemoryBudget::new(6 * 64 * 1024);
        assert!(
            insufficient
                .reservation()
                .try_reserve_archive_json_transform(&large_frame)
                .is_none()
        );
        let sufficient = ProxyMemoryBudget::new(8 * 64 * 1024);
        assert!(
            sufficient
                .reservation()
                .try_reserve_archive_json_transform(&large_frame)
                .is_some()
        );
    }

    #[test]
    fn sse_streaming_envelope_limits_concurrency_and_releases_capacity() {
        let framed_bytes = 8 * UNIT_BYTES;
        let budget = ProxyMemoryBudget::new((framed_bytes * SSE_STREAMING_MEMORY_WEIGHT) as u32);
        let first = budget.reservation();
        let first_permit = first
            .try_reserve_sse_streaming(framed_bytes)
            .expect("one two-copy SSE envelope fits exactly");
        assert_eq!(
            budget.snapshot().0,
            framed_bytes * SSE_STREAMING_MEMORY_WEIGHT
        );

        let second = budget.reservation();
        assert!(second.try_reserve_sse_streaming(framed_bytes).is_none());
        assert!(second.try_reserve_sse_streaming(usize::MAX).is_none());

        drop(first_permit);
        assert_eq!(budget.snapshot().0, 0);
        assert!(second.try_reserve_sse_streaming(framed_bytes).is_some());
    }

    #[test]
    fn default_budget_admits_sixteen_mib_and_refuses_concurrent_large_capture() {
        let budget = ProxyMemoryBudget::new(crate::config::DEFAULT_PROXY_MEMORY_BUDGET_BYTES);
        let request = budget.reservation();
        assert!(request.try_grow(16 * 1024 * 1024, REQUEST_MEMORY_WEIGHT));
        assert!(request.try_grow(16 * 1024 * 1024, 3));
        assert!(request.try_grow(8, 256));
        let response = budget.reservation();
        assert!(!response.try_grow(64 * 1024 * 1024, CAPTURE_MEMORY_WEIGHT));
        assert!(response.try_grow(1024, CAPTURE_MEMORY_WEIGHT));
    }

    #[test]
    fn json_node_expansion_is_rejected_before_deserialization() {
        let budget = ProxyMemoryBudget::new(1024 * 1024);
        let request = budget.reservation();
        let body = format!("[{}0]", "0,".repeat(10_000));
        assert!(request.try_grow(body.len(), REQUEST_MEMORY_WEIGHT));
        assert!(!request.try_reserve_json(body.as_bytes()));
    }

    #[test]
    fn retained_request_partition_guarantees_one_maximum_response_can_progress() {
        let budget = ProxyMemoryBudget::new(crate::config::DEFAULT_PROXY_MEMORY_BUDGET_BYTES);
        let first = budget.reservation();
        assert!(first.try_grow(16 * 1024 * 1024, 3));
        assert!(
            first.finalize_request(tokio::time::Instant::now() + std::time::Duration::from_secs(1))
        );
        let second = budget.reservation();
        assert!(second.try_grow(16 * 1024 * 1024, 3));
        assert!(!second.finalize_request(tokio::time::Instant::now()));
        drop(second);
        assert!(first.reserve_buffered_response(
            MAX_BUFFERED_RESPONSE_BYTES,
            tokio::time::Instant::now() + std::time::Duration::from_secs(1)
        ));
    }

    #[test]
    fn multiple_small_content_length_responses_keep_concurrency() {
        let budget = ProxyMemoryBudget::new(crate::config::DEFAULT_PROXY_MEMORY_BUDGET_BYTES);
        let mut requests = Vec::new();
        for _ in 0..4 {
            let request = budget.reservation();
            assert!(request.try_grow(1024, 3));
            assert!(request
                .finalize_request(tokio::time::Instant::now() + std::time::Duration::from_secs(1)));
            assert!(request.reserve_buffered_response(
                16 * 1024 * 1024,
                tokio::time::Instant::now() + std::time::Duration::from_secs(1)
            ));
            requests.push(request);
        }
        assert_eq!(requests.len(), 4);
    }

    #[test]
    fn response_admission_fails_immediately_without_holding_partial_permits() {
        let budget = ProxyMemoryBudget::new(crate::config::DEFAULT_PROXY_MEMORY_BUDGET_BYTES);
        let held = budget.reservation();
        assert!(held.try_grow(crate::config::DEFAULT_PROXY_MEMORY_BUDGET_BYTES as usize, 1));
        let response = budget.reservation();
        let started = std::time::Instant::now();
        assert!(!response.reserve_buffered_response(16 * 1024 * 1024, tokio::time::Instant::now()));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(!response.has_buffered_response());
        drop(held);
        assert!(response.reserve_buffered_response(
            MAX_BUFFERED_RESPONSE_BYTES,
            tokio::time::Instant::now() + std::time::Duration::from_secs(1)
        ));
    }

    fn assert_no_memory_queue(metrics: &crate::metrics::Metrics) {
        let rendered = metrics.render(&crate::metrics::RuntimeMetrics::default());
        for stage in ["retained", "response", "projection"] {
            assert!(
                rendered.contains(&format!("proxy_memory_waiting{{stage=\"{stage}\"}} 0")),
                "{rendered}"
            );
            for outcome in ["admitted", "timeout", "cancelled"] {
                assert!(
                    rendered.contains(&format!(
                        "proxy_memory_waits_total{{stage=\"{stage}\",outcome=\"{outcome}\"}} 0"
                    )),
                    "{rendered}"
                );
            }
        }
    }

    #[test]
    fn response_admission_fails_immediately_while_capacity_is_held() {
        let budget = ProxyMemoryBudget::new(1024 * 1024);
        let held = budget.reservation();
        assert!(held.try_grow(1024 * 1024, 1));
        let metrics = crate::metrics::Metrics::default();
        for absolute in [1_000_u64, 50] {
            let waiting = budget.reservation();
            let started = std::time::Instant::now();
            assert!(!waiting.reserve_buffered_response(
                1,
                tokio::time::Instant::now() + std::time::Duration::from_millis(absolute),
            ));
            assert!(started.elapsed() < std::time::Duration::from_secs(1));
            assert!(!waiting.has_buffered_response());
        }
        assert_no_memory_queue(&metrics);
        drop(held);
        let waiting = budget.reservation();
        assert!(waiting.reserve_buffered_response(1, tokio::time::Instant::now()));
        drop(waiting);
        assert_eq!(budget.snapshot().0, 0);
    }

    #[test]
    fn response_admission_has_no_queue_to_cancel() {
        let budget = ProxyMemoryBudget::new(1024 * 1024);
        let held = budget.reservation();
        assert!(held.try_grow(1024 * 1024, 1));
        let metrics = crate::metrics::Metrics::default();
        let first = budget.reservation();
        let second = budget.reservation();
        assert!(!first.reserve_buffered_response(128 * 1024, tokio::time::Instant::now()));
        assert!(!second.reserve_buffered_response(64 * 1024, tokio::time::Instant::now()));
        assert!(!first.has_buffered_response());
        assert!(!second.has_buffered_response());
        drop(first);
        drop(held);
        assert!(second.reserve_buffered_response(64 * 1024, tokio::time::Instant::now()));
        assert_eq!(budget.snapshot().0, 3 * 64 * 1024);
        drop(second);
        assert_eq!(budget.snapshot().0, 0);
        assert_no_memory_queue(&metrics);
    }

    #[test]
    fn buffered_response_and_projection_are_one_immediate_reservation() {
        let budget = ProxyMemoryBudget::new(1024 * 1024);
        let held = budget.reservation();
        assert!(held.try_grow(1024 * 1024, 1));
        let metrics = crate::metrics::Metrics::default();
        let request = budget.reservation();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        assert!(
            request
                .reserve_buffered_response_with_projection(
                    256 * 1024,
                    64 * 1024,
                    REQUEST_MEMORY_WEIGHT.saturating_sub(1),
                    deadline,
                )
                .is_none()
        );
        drop(held);
        let projection = request
            .reserve_buffered_response_with_projection(
                256 * 1024,
                64 * 1024,
                REQUEST_MEMORY_WEIGHT.saturating_sub(1),
                deadline,
            )
            .expect("combined response and projection admission");
        assert_eq!(budget.snapshot().0, 14 * 64 * 1024);
        drop(projection);
        assert_eq!(budget.snapshot().0, 12 * 64 * 1024);
        drop(request);
        assert_eq!(budget.snapshot().0, 0);
        assert_no_memory_queue(&metrics);
    }

    #[test]
    fn configured_budget_guarantees_one_buffered_conversation_transition() {
        for budget in [
            crate::config::DEFAULT_PROXY_MEMORY_BUDGET_BYTES as usize,
            512 * 1024 * 1024,
            2_usize * 1024 * 1024 * 1024,
        ] {
            let request = (budget - 1024 * 1024) / 12;
            let rounded = |bytes: usize| bytes.div_ceil(UNIT_BYTES) * UNIT_BYTES;
            let retained_limit = budget / 4 / UNIT_BYTES * UNIT_BYTES;
            assert!(rounded(request.saturating_mul(3)) <= retained_limit);
            assert!(
                rounded(request)
                    + rounded(request.saturating_mul(2))
                    + rounded(MAX_BUFFERED_RESPONSE_BYTES.saturating_mul(CAPTURE_MEMORY_WEIGHT))
                    <= budget / UNIT_BYTES * UNIT_BYTES
            );
        }
    }

    #[tokio::test]
    async fn unexpected_buffered_transition_rejects_extra_native_streams_to_make_progress() {
        const REQUEST_BYTES: usize = 16 * 1024 * 1024;

        let budget = ProxyMemoryBudget::new(crate::config::DEFAULT_PROXY_MEMORY_BUDGET_BYTES);
        let mut streams = Vec::new();
        for _ in 0..4 {
            let stream = budget.reservation();
            assert!(stream.try_grow(REQUEST_BYTES, 1));
            streams.push(stream);
        }
        assert!(
            streams[0]
                .reserve_unexpected_buffered_request(
                    REQUEST_BYTES,
                    REQUEST_MEMORY_WEIGHT.saturating_sub(1),
                )
                .await
        );
        for stream in &streams[1..] {
            assert!(
                !stream
                    .reserve_unexpected_buffered_request(
                        REQUEST_BYTES,
                        REQUEST_MEMORY_WEIGHT.saturating_sub(1),
                    )
                    .await
            );
        }
        assert_eq!(budget.snapshot().2, 3 * REQUEST_BYTES);

        let admitted = streams[0].clone();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        assert!(
            admitted
                .reserve_buffered_response_with_projection(
                    MAX_BUFFERED_RESPONSE_BYTES,
                    REQUEST_BYTES,
                    REQUEST_MEMORY_WEIGHT.saturating_sub(1),
                    deadline,
                )
                .is_none()
        );
        for stream in &streams[1..] {
            stream.release_rejected_request();
        }
        assert_eq!(
            streams.len(),
            4,
            "loser owners remain alive through settlement"
        );
        let projection = admitted
            .reserve_buffered_response_with_projection(
                MAX_BUFFERED_RESPONSE_BYTES,
                REQUEST_BYTES,
                REQUEST_MEMORY_WEIGHT.saturating_sub(1),
                deadline,
            )
            .expect("one unexpected buffered response must make progress");
        assert_eq!(budget.snapshot().0, 240 * 1024 * 1024);
        drop(projection);
        assert_eq!(budget.snapshot().0, 208 * 1024 * 1024);
        admitted.release_retained_request_for_stream();
        assert_eq!(budget.snapshot().2, 0);
        drop(admitted);
        drop(streams);
        assert_eq!(budget.snapshot().0, 0);
    }

    #[test]
    fn projection_fails_while_response_holds_memory_and_releases_transient_permits() {
        let budget = ProxyMemoryBudget::new(1024 * 1024);
        let held = budget.reservation();
        assert!(held.try_grow(1024 * 1024, 1));
        let metrics = crate::metrics::Metrics::default();
        let response = budget.reservation();
        let projection = budget.reservation();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        assert!(!response.reserve_buffered_response(320 * 1024, deadline));
        assert!(
            projection
                .reserve_conversation_projection(
                    64 * 1024,
                    REQUEST_MEMORY_WEIGHT.saturating_sub(1),
                    deadline,
                )
                .is_none()
        );
        drop(held);
        assert!(response.reserve_buffered_response(320 * 1024, deadline));
        assert!(
            projection
                .reserve_conversation_projection(
                    64 * 1024,
                    REQUEST_MEMORY_WEIGHT.saturating_sub(1),
                    deadline,
                )
                .is_none(),
            "projection does not fit beside the buffered response"
        );
        drop(response);
        let permit = projection
            .reserve_conversation_projection(
                64 * 1024,
                REQUEST_MEMORY_WEIGHT.saturating_sub(1),
                deadline,
            )
            .expect("projection fits after the response releases");
        assert_eq!(budget.snapshot().0, 2 * 64 * 1024);
        drop(permit);
        drop(projection);
        assert_eq!(budget.snapshot().0, 0);
        assert_no_memory_queue(&metrics);
    }

    #[test]
    fn projection_admission_fails_immediately_while_capacity_is_held() {
        let budget = ProxyMemoryBudget::new(1024 * 1024);
        let held = budget.reservation();
        assert!(held.try_grow(1024 * 1024, 1));
        let metrics = crate::metrics::Metrics::default();
        let projection = budget.reservation();
        let started = std::time::Instant::now();
        assert!(
            projection
                .reserve_conversation_projection(
                    64 * 1024,
                    REQUEST_MEMORY_WEIGHT.saturating_sub(1),
                    tokio::time::Instant::now() + std::time::Duration::from_millis(50),
                )
                .is_none()
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert_no_memory_queue(&metrics);
        drop(held);
        assert!(
            projection
                .reserve_conversation_projection(
                    64 * 1024,
                    REQUEST_MEMORY_WEIGHT.saturating_sub(1),
                    tokio::time::Instant::now(),
                )
                .is_some()
        );
        drop(projection);
        assert_eq!(budget.snapshot().0, 0);
    }

    #[test]
    fn retained_admission_fails_immediately_until_the_holder_releases() {
        let budget = ProxyMemoryBudget::new(crate::config::DEFAULT_PROXY_MEMORY_BUDGET_BYTES);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        let held = budget.reservation();
        assert!(held.try_grow(16 * 1024 * 1024, 3));
        assert!(held.finalize_request(deadline));

        let too_big = budget.reservation();
        assert!(too_big.try_grow(64 * 1024 * 1024, 1));
        assert!(!too_big.finalize_request(deadline));

        // A smaller request that fits is not stuck behind the failed try.
        let fits = budget.reservation();
        assert!(fits.try_grow(16 * 1024 * 1024, 1));
        assert!(fits.finalize_request(deadline));
        assert_eq!(budget.snapshot().2, 64 * 1024 * 1024);

        let blocked = budget.reservation();
        assert!(blocked.try_grow(16 * 1024 * 1024, 1));
        assert!(!blocked.finalize_request(deadline));
        fits.release_retained_request_for_stream();
        assert!(blocked.finalize_request(deadline));
        drop(held);
        drop(too_big);
        assert_eq!(budget.snapshot().2, 16 * 1024 * 1024);
    }

    #[test]
    fn confirmed_streams_release_retained_partition_without_blocking_buffered_progress() {
        let budget = ProxyMemoryBudget::new(crate::config::DEFAULT_PROXY_MEMORY_BUDGET_BYTES);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        let first_stream = budget.reservation();
        assert!(first_stream.try_grow(16 * 1024 * 1024, 3));
        assert!(first_stream.finalize_request(deadline));
        first_stream.release_retained_request_for_stream();
        let second_stream = budget.reservation();
        assert!(second_stream.try_grow(16 * 1024 * 1024, 3));
        assert!(second_stream.finalize_request(deadline));
        second_stream.release_retained_request_for_stream();

        let buffered = budget.reservation();
        assert!(buffered.try_grow(16 * 1024 * 1024, 3));
        assert!(buffered.finalize_request(deadline));
        assert_eq!(
            budget.snapshot(),
            (
                144 * 1024 * 1024,
                256 * 1024 * 1024,
                48 * 1024 * 1024,
                64 * 1024 * 1024
            )
        );

        assert!(!buffered.reserve_buffered_response(MAX_BUFFERED_RESPONSE_BYTES, deadline));
        assert!(!buffered.has_buffered_response());
        drop(first_stream);
        assert!(!buffered.reserve_buffered_response(MAX_BUFFERED_RESPONSE_BYTES, deadline));
        assert!(!buffered.has_buffered_response());
        drop(second_stream);
        assert!(buffered.reserve_buffered_response(MAX_BUFFERED_RESPONSE_BYTES, deadline));
        assert!(buffered.has_buffered_response());
    }
}
