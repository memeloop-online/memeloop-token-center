//! Per-capture accounting only: no additional storage operation or admission.
use super::*;
use std::time::Instant;

pub(super) struct SpillObservation {
    memory: Arc<ProxyMemoryBudget>,
    workspace: Option<Arc<crate::api::proxy::persistence::Persistence>>,
    staging: ArchiveStagingKey,
    started: Instant,
    attempt_started: Option<Instant>,
    pub(super) ram_bytes: Option<u64>,
    input_bytes: u64,
    began: bool,
    pub(super) completed: bool,
    pub(super) successes: u64,
    pub(super) offloaded_bytes: u64,
    pub(super) no_ram: u64,
    pub(super) workspace_rejections: u64,
    pub(super) other_errors: u64,
    total_await_ms: u64,
    max_await_ms: u64,
    last_ack_ms: Option<u64>,
}

fn millis(start: Instant) -> u64 {
    start.elapsed().as_millis().min(u64::MAX as u128) as u64
}

impl SpillObservation {
    pub(super) fn new(
        memory: Arc<ProxyMemoryBudget>,
        workspace: Option<Arc<crate::api::proxy::persistence::Persistence>>,
        staging: ArchiveStagingKey,
    ) -> Self {
        Self {
            memory,
            workspace,
            staging,
            started: Instant::now(),
            attempt_started: None,
            ram_bytes: Some(0),
            input_bytes: 0,
            began: false,
            completed: false,
            successes: 0,
            offloaded_bytes: 0,
            no_ram: 0,
            workspace_rejections: 0,
            other_errors: 0,
            total_await_ms: 0,
            max_await_ms: 0,
            last_ack_ms: None,
        }
    }

    pub(super) fn accepted(&mut self, bytes: usize) {
        self.ram_bytes = self.ram_bytes.and_then(|old| old.checked_add(bytes as u64));
        self.input_bytes = 0;
    }

    pub(super) fn unobserved_segment_transition(&mut self) {
        // The ordinary request branch can also offload parent/aliased segments.
        // Do not invent per-source RAM accounting for that mixed-input case.
        self.ram_bytes = None;
    }

    pub(super) fn begin(&mut self, input_bytes: usize) {
        self.input_bytes = input_bytes as u64;
        self.attempt_started = Some(Instant::now());
        if !self.began {
            self.began = true;
            self.emit("begin");
        }
    }

    pub(super) fn finish(&mut self, result: &Result<Option<usize>, AppError>) {
        if let Some(started) = self.attempt_started.take() {
            let elapsed = millis(started);
            self.total_await_ms = self.total_await_ms.saturating_add(elapsed);
            self.max_await_ms = self.max_await_ms.max(elapsed);
        }
        match result {
            Ok(Some(bytes)) => {
                self.successes = self.successes.saturating_add(1);
                self.offloaded_bytes = self.offloaded_bytes.saturating_add(*bytes as u64);
                self.ram_bytes = self
                    .ram_bytes
                    .and_then(|old| old.checked_sub(*bytes as u64));
                self.last_ack_ms = Some(millis(self.started));
            }
            Ok(None) => self.no_ram = self.no_ram.saturating_add(1),
            Err(AppError::Storage {
                code: "carrier_workspace_unavailable",
                ..
            }) => {
                self.workspace_rejections = self.workspace_rejections.saturating_add(1);
            }
            Err(_) => self.other_errors = self.other_errors.saturating_add(1),
        }
    }

    fn emit(&self, phase: &'static str) {
        let source = self.memory.snapshot();
        let workspace = self
            .workspace
            .as_ref()
            .map(|owner| owner.upload_workspace.snapshot());
        let request_id = matches!(
            self.staging.owner,
            crate::archive_staging::ArchiveStagingOwner::ProxyRequest(_)
        )
        .then(|| self.staging.owner.id().to_string());
        let current_await_ms = self.attempt_started.map(millis);
        tracing::info!(
            phase = "credited_carrier_spill",
            observation = phase,
            request_id = request_id.as_deref(),
            owner_kind = self.staging.owner.kind(),
            owner_id = %self.staging.owner.id(),
            source_ram_payload_bytes = self.ram_bytes,
            source_ram_payload_bytes_known = self.ram_bytes.is_some(),
            awaiting_input_payload_bytes = self.input_bytes,
            shared_source_used_bytes = source.0,
            shared_source_limit_bytes = source.1,
            shared_retained_used_bytes = source.2,
            shared_retained_limit_bytes = source.3,
            shared_workspace_used_bytes = workspace.map(|snapshot| snapshot.0),
            shared_workspace_limit_bytes = workspace.map(|snapshot| snapshot.1),
            spill_inflight = self.attempt_started.is_some(),
            spill_successes = self.successes,
            spill_confirmed_bytes = self.offloaded_bytes,
            spill_no_ram = self.no_ram,
            spill_workspace_rejections = self.workspace_rejections,
            spill_other_errors = self.other_errors,
            spill_total_await_ms = self.total_await_ms.saturating_add(current_await_ms.unwrap_or(0)),
            spill_max_await_ms = self.max_await_ms.max(current_await_ms.unwrap_or(0)),
            current_spill_await_ms = current_await_ms,
            last_confirmed_ack_elapsed_ms = self.last_ack_ms,
            capture_elapsed_ms = millis(self.started),
            capture_eof = self.completed,
            "credited capture spill observation"
        );
    }
}

impl Drop for SpillObservation {
    fn drop(&mut self) {
        if self.began {
            self.emit("final");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spill_accounting_counts_only_confirmed_segment_transitions() {
        let memory = Arc::new(ProxyMemoryBudget::new(512 * 1024));
        let key = ArchiveStagingKey::new(
            crate::archive_staging::ArchiveStagingOwner::ProxyRequest(uuid::Uuid::now_v7()),
            crate::archive_staging::ArchiveStagingPurpose::Response,
            uuid::Uuid::now_v7(),
        )
        .unwrap();
        let mut observation = SpillObservation::new(memory, None, key);
        observation.accepted(64 * 1024);
        observation.begin(4096);
        assert!(observation.attempt_started.is_some());
        observation.finish(&Err(AppError::storage("carrier_workspace_unavailable")));
        assert_eq!(observation.workspace_rejections, 1);
        assert_eq!(observation.ram_bytes, Some(64 * 1024));
        assert_eq!(observation.offloaded_bytes, 0);
        assert!(observation.last_ack_ms.is_none());
        observation.begin(4096);
        observation.finish(&Err(AppError::storage("carrier_stage_identity_mismatch")));
        assert_eq!(observation.other_errors, 1);
        assert!(observation.last_ack_ms.is_none());
        observation.begin(4096);
        observation.finish(&Ok(Some(64 * 1024)));
        assert_eq!(observation.successes, 1);
        assert_eq!(observation.offloaded_bytes, 64 * 1024);
        assert_eq!(observation.ram_bytes, Some(0));
        assert!(observation.last_ack_ms.is_some());
        observation.begin(4096);
        observation.finish(&Ok(None));
        assert_eq!(observation.no_ram, 1);
        assert_eq!(observation.successes, 1);
        observation.unobserved_segment_transition();
        observation.accepted(4096);
        assert_eq!(observation.ram_bytes, None, "mixed inputs remain unknown");
    }
}
