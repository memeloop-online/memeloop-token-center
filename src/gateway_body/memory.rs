use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

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
    response_wait_started: Arc<tokio::sync::Notify>,
}

impl ProxyMemoryBudget {
    pub(crate) fn new(bytes: u32) -> Self {
        Self {
            capacity: bytes as usize,
            permits: Arc::new(Semaphore::new(bytes as usize / UNIT_BYTES)),
            retained_requests: Arc::new(Semaphore::new(bytes as usize / 4 / UNIT_BYTES)),
            #[cfg(test)]
            response_wait_started: Arc::new(tokio::sync::Notify::new()),
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
            #[cfg(test)]
            response_wait_started: self.response_wait_started.clone(),
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
    pub(crate) async fn wait_for_response_reservation_for_test(&self) {
        self.response_wait_started.notified().await;
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
    #[cfg(test)]
    response_wait_started: Arc<tokio::sync::Notify>,
}

impl ProxyMemoryReservation {
    pub(crate) fn has_buffered_response(&self) -> bool {
        self.response_reserved.load(Ordering::Acquire)
    }

    pub(crate) fn response_capture_fits(&self, bytes: usize) -> bool {
        bytes.saturating_mul(CAPTURE_MEMORY_WEIGHT) <= self.response_bytes.load(Ordering::Acquire)
    }

    pub(crate) fn try_finalize_request(&self) -> bool {
        let Ok(held) = self.held.lock() else {
            return false;
        };
        let Ok(units) = u32::try_from(held.0.div_ceil(UNIT_BYTES)) else {
            return false;
        };
        let Ok(permit) = self.retained_requests.clone().try_acquire_many_owned(units) else {
            return false;
        };
        let Ok(mut retained) = self.retained.lock() else {
            return false;
        };
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
        #[cfg(test)]
        self.response_wait_started.notify_one();
        let Ok(permit) =
            tokio::time::timeout_at(deadline, self.permits.clone().acquire_many_owned(units)).await
        else {
            return false;
        };
        let Ok(permit) = permit else {
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
        assert!(first.try_finalize_request());
        let second = budget.reservation();
        assert!(second.try_grow(16 * 1024 * 1024, 3));
        assert!(!second.try_finalize_request());
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
            assert!(request.try_finalize_request());
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
}
