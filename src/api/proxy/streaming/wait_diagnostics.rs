use std::{future::Future, time::Duration};

#[derive(Default)]
pub(super) struct WaitDiagnostics {
    upstream_pending: Duration,
    downstream_pending: Duration,
    max_upstream_pending: Duration,
    max_downstream_pending: Duration,
    no_body_pending: Duration,
    max_no_body_pending: Duration,
    body_chunks: u64,
    heartbeats: u64,
    transport_terminal: Option<&'static str>,
}

impl WaitDiagnostics {
    pub(super) async fn upstream<T>(&mut self, future: impl Future<Output = T>) -> T {
        self.observe(future, true).await
    }

    pub(super) async fn downstream<T>(&mut self, future: impl Future<Output = T>) -> T {
        self.observe(future, false).await
    }

    async fn observe<T>(&mut self, future: impl Future<Output = T>, upstream: bool) -> T {
        let mut pending = PendingWait {
            diagnostics: self,
            upstream,
            started: None,
        };
        tokio::pin!(future);
        std::future::poll_fn(|context| {
            let result = future.as_mut().poll(context);
            if result.is_pending() && pending.started.is_none() {
                pending.started = Some(tokio::time::Instant::now());
            }
            result
        })
        .await
    }

    pub(super) fn body_chunk(&mut self) {
        self.body_chunks = self.body_chunks.saturating_add(1);
        self.no_body_pending = Duration::ZERO;
    }

    pub(super) fn heartbeat(&mut self) {
        self.heartbeats = self.heartbeats.saturating_add(1);
    }

    pub(super) fn terminal(&mut self, outcome: &'static str) {
        self.transport_terminal = Some(outcome);
    }

    pub(super) fn emit(&self, request_id: uuid::Uuid, owner_outcome: &'static str) {
        tracing::info!(
            %request_id,
            stage = "stream_wait_summary",
            observation_layer = "application_body_poll",
            http2_data_silence_proven = false,
            owner_outcome,
            transport_terminal = self.transport_terminal.unwrap_or("not_observed"),
            upstream_pending_ms = millis(self.upstream_pending),
            max_upstream_pending_ms = millis(self.max_upstream_pending),
            max_no_body_pending_lower_bound_ms = millis(self.max_no_body_pending),
            downstream_capacity_pending_ms = millis(self.downstream_pending),
            max_downstream_capacity_pending_ms = millis(self.max_downstream_pending),
            nonempty_body_chunks = self.body_chunks,
            progress_heartbeats_enqueued = self.heartbeats,
            "stream application wait observations"
        );
    }
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

struct PendingWait<'a> {
    diagnostics: &'a mut WaitDiagnostics,
    upstream: bool,
    started: Option<tokio::time::Instant>,
}

impl Drop for PendingWait<'_> {
    fn drop(&mut self) {
        let Some(started) = self.started else { return };
        let elapsed = started.elapsed();
        if self.upstream {
            self.diagnostics.upstream_pending += elapsed;
            self.diagnostics.max_upstream_pending =
                self.diagnostics.max_upstream_pending.max(elapsed);
            self.diagnostics.no_body_pending += elapsed;
            self.diagnostics.max_no_body_pending = self
                .diagnostics
                .max_no_body_pending
                .max(self.diagnostics.no_body_pending);
        } else {
            self.diagnostics.downstream_pending += elapsed;
            self.diagnostics.max_downstream_pending =
                self.diagnostics.max_downstream_pending.max(elapsed);
        }
    }
}

#[cfg(test)]
mod tests;
