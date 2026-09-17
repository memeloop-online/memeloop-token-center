use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::task::Poll;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

// Charge raw bytes, bounded JSON trees/clones and encrypted batch copies before
// retaining input. This is a process-wide admission budget, not a metric.
pub(crate) const REQUEST_MEMORY_WEIGHT: usize = 3;
pub(crate) const CAPTURE_MEMORY_WEIGHT: usize = 3;
pub(crate) const MAX_BUFFERED_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
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
    #[cfg(test)]
    retained_wait_started: Arc<tokio::sync::Notify>,
    #[cfg(test)]
    response_wait_started: Arc<tokio::sync::Notify>,
    #[cfg(test)]
    projection_wait_started: Arc<tokio::sync::Notify>,
}

impl ProxyMemoryBudget {
    pub(crate) fn new(bytes: u32) -> Self {
        Self {
            capacity: bytes as usize,
            permits: Arc::new(Semaphore::new(bytes as usize / UNIT_BYTES)),
            retained_requests: Arc::new(Semaphore::new(bytes as usize / 4 / UNIT_BYTES)),
            #[cfg(test)]
            retained_wait_started: Arc::new(tokio::sync::Notify::new()),
            #[cfg(test)]
            response_wait_started: Arc::new(tokio::sync::Notify::new()),
            #[cfg(test)]
            projection_wait_started: Arc::new(tokio::sync::Notify::new()),
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
            admission: OnceLock::new(),
            #[cfg(test)]
            retained_wait_started: self.retained_wait_started.clone(),
            #[cfg(test)]
            response_wait_started: self.response_wait_started.clone(),
            #[cfg(test)]
            projection_wait_started: self.projection_wait_started.clone(),
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
            return Err(crate::error::AppError::Overloaded);
        }
        Ok(reservation)
    }

    #[cfg(test)]
    pub(crate) async fn wait_for_retained_reservation_for_test(&self) {
        self.retained_wait_started.notified().await;
    }

    #[cfg(test)]
    pub(crate) async fn wait_for_response_reservation_for_test(&self) {
        self.response_wait_started.notified().await;
    }

    #[cfg(test)]
    pub(crate) async fn wait_for_projection_reservation_for_test(&self) {
        self.projection_wait_started.notified().await;
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
    admission: OnceLock<(std::time::Duration, crate::metrics::Metrics)>,
    #[cfg(test)]
    retained_wait_started: Arc<tokio::sync::Notify>,
    #[cfg(test)]
    response_wait_started: Arc<tokio::sync::Notify>,
    #[cfg(test)]
    projection_wait_started: Arc<tokio::sync::Notify>,
}

pub(crate) struct ConversationProjectionPermit {
    _permit: OwnedSemaphorePermit,
}

pub(crate) struct ArchiveOutputMemory {
    reservation: Arc<ProxyMemoryReservation>,
    bytes: usize,
}

impl Drop for ArchiveOutputMemory {
    fn drop(&mut self) {
        self.reservation.release(self.bytes, 1);
    }
}

impl ProxyMemoryReservation {
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

    /// Snapshot the selected account's policy for this request. Retries cannot
    /// replace its selected queue policy or metrics owner.
    pub(crate) fn configure_admission(
        &self,
        wait: std::time::Duration,
        metrics: crate::metrics::Metrics,
    ) {
        let _ = self.admission.set((wait, metrics));
    }

    async fn acquire(
        &self,
        semaphore: &Arc<Semaphore>,
        units: u32,
        deadline: tokio::time::Instant,
        stage: crate::metrics::memory_admission::Stage,
    ) -> Option<OwnedSemaphorePermit> {
        let wait = self.admission.get().map(|(wait, _)| *wait).unwrap_or(
            std::time::Duration::from_millis(
                crate::provider::CodexTransportPolicy::default().memory_admission_wait_millis,
            ),
        );
        let deadline = deadline.min(tokio::time::Instant::now() + wait);
        // All admissions use Tokio's fair asynchronous queue. A try-acquire
        // fast path can overtake an already queued multi-permit waiter.
        let mut observation = None;
        #[cfg(test)]
        let mut async_wait_observed = false;
        let permit = {
            let mut acquire = Box::pin(semaphore.clone().acquire_many_owned(units));
            tokio::time::timeout_at(
                deadline,
                std::future::poll_fn(|context| match acquire.as_mut().poll(context) {
                    Poll::Ready(permit) => Poll::Ready(permit),
                    Poll::Pending => {
                        #[cfg(test)]
                        if !async_wait_observed {
                            async_wait_observed = true;
                            match stage {
                                crate::metrics::memory_admission::Stage::Response => {
                                    self.response_wait_started.notify_one();
                                }
                                crate::metrics::memory_admission::Stage::Projection => {
                                    self.projection_wait_started.notify_one();
                                }
                                crate::metrics::memory_admission::Stage::Retained => {}
                            }
                        }
                        if observation.is_none() {
                            observation = self
                                .admission
                                .get()
                                .map(|(_, metrics)| metrics.proxy_memory_wait(stage));
                        }
                        Poll::Pending
                    }
                }),
            )
            .await
            .ok()
            .and_then(Result::ok)
        };
        if let Some(observation) = observation {
            observation.finish(permit.is_some());
        }
        permit
    }

    pub(crate) fn has_buffered_response(&self) -> bool {
        self.response_reserved.load(Ordering::Acquire)
    }

    pub(crate) fn response_capture_fits(&self, bytes: usize) -> bool {
        bytes.saturating_mul(CAPTURE_MEMORY_WEIGHT) <= self.response_bytes.load(Ordering::Acquire)
    }

    pub(crate) async fn finalize_request(&self, deadline: tokio::time::Instant) -> bool {
        let units = {
            let Ok(held) = self.held.lock() else {
                return false;
            };
            let Ok(units) = u32::try_from(held.0.div_ceil(UNIT_BYTES)) else {
                return false;
            };
            units
        };
        #[cfg(test)]
        self.retained_wait_started.notify_one();
        let Some(permit) = self
            .acquire(
                &self.retained_requests,
                units,
                deadline,
                crate::metrics::memory_admission::Stage::Retained,
            )
            .await
        else {
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
    /// body plus terminal projection footprint without queuing; losers must
    /// fail and release their raw bytes so the admitted transition can make
    /// progress in the process-wide FIFO queue.
    pub(crate) async fn reserve_unexpected_buffered_request(
        &self,
        projection_bytes: usize,
    ) -> bool {
        let units = {
            let Ok(held) = self.held.lock() else {
                return false;
            };
            let Some(bytes) = projection_bytes
                .checked_mul(REQUEST_MEMORY_WEIGHT.saturating_sub(1))
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

    pub(crate) async fn reserve_buffered_response(
        &self,
        maximum: usize,
        deadline: tokio::time::Instant,
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
        let Some(permit) = self
            .acquire(
                &self.permits,
                units,
                deadline,
                crate::metrics::memory_admission::Stage::Response,
            )
            .await
        else {
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
    /// projection as one fair allocation. Acquiring them separately can
    /// deadlock when an earlier response owner blocks the next response while
    /// its own projection waits behind that response in the FIFO queue.
    pub(crate) async fn reserve_buffered_response_with_projection(
        &self,
        maximum: usize,
        projection_bytes: usize,
        deadline: tokio::time::Instant,
    ) -> Option<ConversationProjectionPermit> {
        if self.has_buffered_response() || maximum > MAX_BUFFERED_RESPONSE_BYTES {
            return None;
        }
        let response_bytes = maximum
            .saturating_mul(CAPTURE_MEMORY_WEIGHT)
            .max(UNIT_BYTES);
        let response_units = u32::try_from(response_bytes.div_ceil(UNIT_BYTES)).ok()?;
        let projection_bytes =
            projection_bytes.checked_mul(REQUEST_MEMORY_WEIGHT.saturating_sub(1))?;
        let projection_units = u32::try_from(projection_bytes.div_ceil(UNIT_BYTES)).ok()?;
        let total_units = response_units.checked_add(projection_units)?;
        let mut permit = self
            .acquire(
                &self.permits,
                total_units,
                deadline,
                crate::metrics::memory_admission::Stage::Response,
            )
            .await?;
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

    pub(crate) fn response_estimate_fits(&self, bytes: usize, nodes: usize) -> bool {
        bytes
            .saturating_mul(2)
            .saturating_add(nodes.saturating_mul(256))
            <= self.response_bytes.load(Ordering::Acquire)
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
    /// Terminal database settlement must not keep unrelated memory waiters
    /// blocked merely because it still borrows the request record.
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

    /// Fairly reserve the transient JSON-tree copies required to project a
    /// retained raw conversation body. The returned permit is independent of
    /// the long-lived request reservation and never occupies the buffered
    /// retained-request partition.
    pub(crate) async fn reserve_conversation_projection(
        &self,
        bytes: usize,
        deadline: tokio::time::Instant,
    ) -> Option<ConversationProjectionPermit> {
        let weighted = bytes.checked_mul(REQUEST_MEMORY_WEIGHT.saturating_sub(1))?;
        let units = u32::try_from(weighted.div_ceil(UNIT_BYTES)).ok()?;
        self.acquire(
            &self.permits,
            units,
            deadline,
            crate::metrics::memory_admission::Stage::Projection,
        )
        .await
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
    serde_json::to_writer(&mut count, value).map_err(|_| crate::error::AppError::Internal)?;
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

    #[tokio::test]
    async fn retained_request_partition_guarantees_one_maximum_response_can_progress() {
        let budget = ProxyMemoryBudget::new(crate::config::DEFAULT_PROXY_MEMORY_BUDGET_BYTES);
        let first = budget.reservation();
        assert!(first.try_grow(16 * 1024 * 1024, 3));
        assert!(
            first
                .finalize_request(tokio::time::Instant::now() + std::time::Duration::from_secs(1))
                .await
        );
        let second = budget.reservation();
        assert!(second.try_grow(16 * 1024 * 1024, 3));
        assert!(!second.finalize_request(tokio::time::Instant::now()).await);
        drop(second);
        assert!(
            first
                .reserve_buffered_response(
                    MAX_BUFFERED_RESPONSE_BYTES,
                    tokio::time::Instant::now() + std::time::Duration::from_secs(1)
                )
                .await
        );
        assert!(first.response_estimate_fits(MAX_BUFFERED_RESPONSE_BYTES, 8));
    }

    #[tokio::test]
    async fn multiple_small_content_length_responses_keep_concurrency() {
        let budget = ProxyMemoryBudget::new(crate::config::DEFAULT_PROXY_MEMORY_BUDGET_BYTES);
        let mut requests = Vec::new();
        for _ in 0..4 {
            let request = budget.reservation();
            assert!(request.try_grow(1024, 3));
            assert!(
                request
                    .finalize_request(
                        tokio::time::Instant::now() + std::time::Duration::from_secs(1)
                    )
                    .await
            );
            assert!(
                request
                    .reserve_buffered_response(
                        16 * 1024 * 1024,
                        tokio::time::Instant::now() + std::time::Duration::from_secs(1)
                    )
                    .await
            );
            requests.push(request);
        }
        assert_eq!(requests.len(), 4);
    }

    #[tokio::test]
    async fn response_wait_is_bounded_without_holding_partial_response_permits() {
        let budget = ProxyMemoryBudget::new(crate::config::DEFAULT_PROXY_MEMORY_BUDGET_BYTES);
        let held = budget.reservation();
        assert!(held.try_grow(crate::config::DEFAULT_PROXY_MEMORY_BUDGET_BYTES as usize, 1));
        let response = budget.reservation();
        assert!(
            !response
                .reserve_buffered_response(16 * 1024 * 1024, tokio::time::Instant::now())
                .await
        );
        assert!(!response.has_buffered_response());
        drop(held);
        assert!(
            response
                .reserve_buffered_response(
                    MAX_BUFFERED_RESPONSE_BYTES,
                    tokio::time::Instant::now() + std::time::Duration::from_secs(1)
                )
                .await
        );
    }

    #[tokio::test(start_paused = true)]
    async fn configured_wait_and_absolute_deadline_bound_response_admission() {
        let budget = ProxyMemoryBudget::new(1024 * 1024);
        let held = budget.reservation();
        assert!(held.try_grow(1024 * 1024, 1));
        let metrics = crate::metrics::Metrics::default();
        for (configured, absolute, expected) in [(100, 1000, 100), (1000, 50, 50)] {
            let waiting = budget.reservation();
            waiting.configure_admission(
                std::time::Duration::from_millis(configured),
                metrics.clone(),
            );
            let started = tokio::time::Instant::now();
            assert!(
                !waiting
                    .reserve_buffered_response(
                        1,
                        started + std::time::Duration::from_millis(absolute)
                    )
                    .await
            );
            assert_eq!(
                started.elapsed(),
                std::time::Duration::from_millis(expected)
            );
            assert!(!waiting.has_buffered_response());
        }
        let rendered = metrics.render(&crate::metrics::RuntimeMetrics::default());
        assert!(
            rendered.contains("proxy_memory_waits_total{stage=\"response\",outcome=\"timeout\"} 2")
        );
        assert!(rendered.contains("proxy_memory_waiting{stage=\"response\"} 0"));
        drop(held);
        assert_eq!(budget.snapshot().0, 0);
    }

    #[tokio::test]
    async fn response_queue_cancellation_releases_fifo_head_and_all_permits() {
        let budget = ProxyMemoryBudget::new(1024 * 1024);
        let held = budget.reservation();
        assert!(held.try_grow(1024 * 1024, 1));
        let metrics = crate::metrics::Metrics::default();
        let mut waiters = Vec::new();
        for maximum in [128 * 1024, 64 * 1024] {
            let waiting = budget.reservation();
            waiting.configure_admission(std::time::Duration::from_secs(5), metrics.clone());
            waiters.push(tokio::spawn(async move {
                assert!(
                    waiting
                        .reserve_buffered_response(
                            maximum,
                            tokio::time::Instant::now() + std::time::Duration::from_secs(5)
                        )
                        .await
                );
                waiting
            }));
            budget.wait_for_response_reservation_for_test().await;
        }
        let first = waiters.remove(0);
        first.abort();
        assert!(first.await.err().unwrap().is_cancelled());
        drop(held);
        let second = waiters.remove(0).await.unwrap();
        assert_eq!(budget.snapshot().0, 3 * 64 * 1024);
        drop(second);
        assert_eq!(budget.snapshot().0, 0);
        let rendered = metrics.render(&crate::metrics::RuntimeMetrics::default());
        assert!(
            rendered
                .contains("proxy_memory_waits_total{stage=\"response\",outcome=\"cancelled\"} 1")
        );
        assert!(
            rendered
                .contains("proxy_memory_waits_total{stage=\"response\",outcome=\"admitted\"} 1")
        );
        assert!(rendered.contains("proxy_memory_waiting{stage=\"response\"} 0"));
    }

    #[tokio::test]
    async fn buffered_response_and_projection_share_one_fair_admission() {
        let budget = ProxyMemoryBudget::new(1024 * 1024);
        let held = budget.reservation();
        assert!(held.try_grow(1024 * 1024, 1));
        let metrics = crate::metrics::Metrics::default();
        let request = budget.reservation();
        request.configure_admission(std::time::Duration::from_secs(5), metrics.clone());
        let waiting = request.clone();
        let task = tokio::spawn(async move {
            let projection = waiting
                .reserve_buffered_response_with_projection(
                    256 * 1024,
                    64 * 1024,
                    tokio::time::Instant::now() + std::time::Duration::from_secs(5),
                )
                .await
                .expect("combined response and projection admission");
            (waiting, projection)
        });
        budget.wait_for_response_reservation_for_test().await;
        assert!(!task.is_finished());

        drop(held);
        let (waiting, projection) = task.await.unwrap();
        assert_eq!(budget.snapshot().0, 14 * 64 * 1024);
        drop(projection);
        assert_eq!(budget.snapshot().0, 12 * 64 * 1024);
        drop(waiting);
        drop(request);
        assert_eq!(budget.snapshot().0, 0);

        let rendered = metrics.render(&crate::metrics::RuntimeMetrics::default());
        assert!(
            rendered
                .contains("proxy_memory_waits_total{stage=\"response\",outcome=\"admitted\"} 1")
        );
        assert!(rendered.contains("proxy_memory_waiting{stage=\"response\"} 0"));
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
                .reserve_unexpected_buffered_request(REQUEST_BYTES)
                .await
        );
        for stream in &streams[1..] {
            assert!(
                !stream
                    .reserve_unexpected_buffered_request(REQUEST_BYTES)
                    .await
            );
        }
        assert_eq!(budget.snapshot().2, 3 * REQUEST_BYTES);

        let admitted = streams[0].clone();
        admitted.configure_admission(
            std::time::Duration::from_secs(5),
            crate::metrics::Metrics::default(),
        );
        let waiting = admitted.clone();
        let task = tokio::spawn(async move {
            let projection = waiting
                .reserve_buffered_response_with_projection(
                    MAX_BUFFERED_RESPONSE_BYTES,
                    REQUEST_BYTES,
                    tokio::time::Instant::now() + std::time::Duration::from_secs(5),
                )
                .await
                .expect("one unexpected buffered response must make progress");
            (waiting, projection)
        });
        budget.wait_for_response_reservation_for_test().await;
        assert!(!task.is_finished());

        for stream in &streams[1..] {
            stream.release_rejected_request();
        }
        assert_eq!(
            streams.len(),
            4,
            "loser owners remain alive through settlement"
        );
        let (waiting, projection) = task.await.unwrap();
        assert_eq!(budget.snapshot().0, 240 * 1024 * 1024);
        drop(projection);
        assert_eq!(budget.snapshot().0, 208 * 1024 * 1024);
        waiting.release_retained_request_for_stream();
        assert_eq!(budget.snapshot().2, 0);
        drop(waiting);
        drop(admitted);
        drop(streams);
        assert_eq!(budget.snapshot().0, 0);
    }

    #[tokio::test]
    async fn projection_waits_behind_buffered_response_and_releases_transient_permits() {
        let budget = ProxyMemoryBudget::new(1024 * 1024);
        let held = budget.reservation();
        assert!(held.try_grow(1024 * 1024, 1));
        let metrics = crate::metrics::Metrics::default();

        let response = budget.reservation();
        response.configure_admission(std::time::Duration::from_secs(5), metrics.clone());
        let response_task = tokio::spawn(async move {
            assert!(
                response
                    .reserve_buffered_response(
                        320 * 1024,
                        tokio::time::Instant::now() + std::time::Duration::from_secs(5),
                    )
                    .await
            );
            response
        });
        budget.wait_for_response_reservation_for_test().await;

        let projection = budget.reservation();
        projection.configure_admission(std::time::Duration::from_secs(5), metrics.clone());
        let projection_task = tokio::spawn(async move {
            let permit = projection
                .reserve_conversation_projection(
                    64 * 1024,
                    tokio::time::Instant::now() + std::time::Duration::from_secs(5),
                )
                .await
                .expect("projection enters the fair lifecycle queue");
            (projection, permit)
        });
        budget.wait_for_projection_reservation_for_test().await;
        assert!(!response_task.is_finished());
        assert!(!projection_task.is_finished());

        drop(held);
        let response = response_task.await.unwrap();
        assert!(
            !projection_task.is_finished(),
            "the later projection cannot overtake the buffered response"
        );
        drop(response);
        let (projection, permit) = projection_task.await.unwrap();
        assert_eq!(budget.snapshot().0, 2 * 64 * 1024);
        drop(permit);
        drop(projection);
        assert_eq!(budget.snapshot().0, 0);

        let rendered = metrics.render(&crate::metrics::RuntimeMetrics::default());
        assert!(
            rendered
                .contains("proxy_memory_waits_total{stage=\"response\",outcome=\"admitted\"} 1")
        );
        assert!(
            rendered
                .contains("proxy_memory_waits_total{stage=\"projection\",outcome=\"admitted\"} 1")
        );
        assert!(rendered.contains("proxy_memory_waiting{stage=\"projection\"} 0"));
    }

    #[tokio::test(start_paused = true)]
    async fn projection_wait_is_bounded_by_the_lifecycle_deadline() {
        let budget = ProxyMemoryBudget::new(1024 * 1024);
        let held = budget.reservation();
        assert!(held.try_grow(1024 * 1024, 1));
        let metrics = crate::metrics::Metrics::default();
        let projection = budget.reservation();
        projection.configure_admission(std::time::Duration::from_secs(1), metrics.clone());
        let started = tokio::time::Instant::now();
        assert!(
            projection
                .reserve_conversation_projection(
                    64 * 1024,
                    started + std::time::Duration::from_millis(50),
                )
                .await
                .is_none()
        );
        assert_eq!(started.elapsed(), std::time::Duration::from_millis(50));
        let rendered = metrics.render(&crate::metrics::RuntimeMetrics::default());
        assert!(
            rendered
                .contains("proxy_memory_waits_total{stage=\"projection\",outcome=\"timeout\"} 1")
        );
        assert!(rendered.contains("proxy_memory_waiting{stage=\"projection\"} 0"));
        drop(held);
        drop(projection);
        assert_eq!(budget.snapshot().0, 0);
    }

    #[tokio::test]
    async fn retained_wait_is_fifo_and_bounded_before_upstream_dispatch() {
        let budget = ProxyMemoryBudget::new(crate::config::DEFAULT_PROXY_MEMORY_BUDGET_BYTES);
        let held = budget.reservation();
        assert!(held.try_grow(16 * 1024 * 1024, 3));
        assert!(
            held.finalize_request(tokio::time::Instant::now() + std::time::Duration::from_secs(1))
                .await
        );
        budget.wait_for_retained_reservation_for_test().await;
        let first_waiter = budget.reservation();
        assert!(first_waiter.try_grow(64 * 1024 * 1024, 1));
        let waiting = first_waiter.clone();
        let first_task = tokio::spawn(async move {
            waiting
                .finalize_request(tokio::time::Instant::now() + std::time::Duration::from_secs(1))
                .await
        });
        budget.wait_for_retained_reservation_for_test().await;
        let second_waiter = budget.reservation();
        assert!(second_waiter.try_grow(16 * 1024 * 1024, 1));
        let waiting = second_waiter.clone();
        let second_task = tokio::spawn(async move {
            waiting
                .finalize_request(tokio::time::Instant::now() + std::time::Duration::from_secs(1))
                .await
        });
        budget.wait_for_retained_reservation_for_test().await;
        assert!(!first_task.is_finished());
        assert!(!second_task.is_finished());
        drop(held);
        assert!(first_task.await.expect("first retained admission task"));
        tokio::task::yield_now().await;
        assert!(!second_task.is_finished());
        first_waiter.release_retained_request_for_stream();
        assert!(second_task.await.expect("second retained admission task"));
        assert_eq!(budget.snapshot().2, 16 * 1024 * 1024);
    }

    #[tokio::test]
    async fn confirmed_streams_release_retained_partition_without_blocking_buffered_progress() {
        let budget = ProxyMemoryBudget::new(crate::config::DEFAULT_PROXY_MEMORY_BUDGET_BYTES);
        let first_stream = budget.reservation();
        assert!(first_stream.try_grow(16 * 1024 * 1024, 3));
        assert!(
            first_stream
                .finalize_request(tokio::time::Instant::now() + std::time::Duration::from_secs(1))
                .await
        );
        first_stream.release_retained_request_for_stream();
        let second_stream = budget.reservation();
        assert!(second_stream.try_grow(16 * 1024 * 1024, 3));
        assert!(
            second_stream
                .finalize_request(tokio::time::Instant::now() + std::time::Duration::from_secs(1))
                .await
        );
        second_stream.release_retained_request_for_stream();

        let buffered = budget.reservation();
        assert!(buffered.try_grow(16 * 1024 * 1024, 3));
        assert!(
            buffered
                .finalize_request(tokio::time::Instant::now() + std::time::Duration::from_secs(1))
                .await
        );
        assert_eq!(
            budget.snapshot(),
            (
                144 * 1024 * 1024,
                256 * 1024 * 1024,
                48 * 1024 * 1024,
                64 * 1024 * 1024
            )
        );

        let waiting = buffered.clone();
        let task = tokio::spawn(async move {
            waiting
                .reserve_buffered_response(
                    MAX_BUFFERED_RESPONSE_BYTES,
                    tokio::time::Instant::now() + std::time::Duration::from_secs(1),
                )
                .await
        });
        budget.wait_for_response_reservation_for_test().await;
        assert!(!task.is_finished());
        drop(first_stream);
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        drop(second_stream);
        assert!(task.await.expect("buffered response admission task"));
        assert!(buffered.has_buffered_response());
    }
}
