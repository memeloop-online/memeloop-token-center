use super::*;
use crate::metrics::{UpstreamHealthEvent, UpstreamHealthReason};

#[derive(Clone, Copy)]
pub(super) enum UpstreamAttemptTerminal {
    Succeeded,
    Inconclusive,
    Failed {
        kind: UpstreamFailureKind,
        reason: UpstreamHealthReason,
    },
}

impl UpstreamAttemptTerminal {
    pub(super) const fn invalid_response() -> Self {
        Self::Failed {
            kind: UpstreamFailureKind::InvalidResponse,
            reason: UpstreamHealthReason::InvalidResponse,
        }
    }
}

/// Owns the half-open circuit-breaker transition after an HTTP response has
/// been admitted. A 2xx header is not recovery: only the buffered or streaming
/// terminal path can confirm protocol validity and durable settlement.
#[must_use]
pub(super) struct UpstreamAttemptGuard {
    state: Option<AppState>,
    request_id: Uuid,
    upstream_account_id: Uuid,
    admission: UpstreamAttemptAdmission,
}

impl UpstreamAttemptGuard {
    pub(super) fn new(
        state: &AppState,
        request_id: Uuid,
        upstream_account_id: Uuid,
        admission: UpstreamAttemptAdmission,
    ) -> Self {
        debug_assert_ne!(admission, UpstreamAttemptAdmission::Unavailable);
        Self {
            state: Some(state.clone()),
            request_id,
            upstream_account_id,
            admission,
        }
    }

    pub(super) async fn complete(&mut self, terminal: UpstreamAttemptTerminal) {
        let Some(state) = self.state.take() else {
            return;
        };
        record_terminal(
            state,
            self.request_id,
            self.upstream_account_id,
            self.admission,
            terminal,
        )
        .await;
    }
}

impl Drop for UpstreamAttemptGuard {
    fn drop(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        let request_id = self.request_id;
        let upstream_account_id = self.upstream_account_id;
        let admission = self.admission;
        // Proxy guards are created and dropped on the Tokio request runtime.
        // Cancellation cannot prove either upstream failure or recovery. It
        // must not make a half-open account healthy, and it must not let a
        // downstream disconnect poison an otherwise healthy account.
        tokio::spawn(async move {
            record_terminal(
                state,
                request_id,
                upstream_account_id,
                admission,
                UpstreamAttemptTerminal::Inconclusive,
            )
            .await;
        });
    }
}

async fn record_terminal(
    state: AppState,
    request_id: Uuid,
    upstream_account_id: Uuid,
    admission: UpstreamAttemptAdmission,
    terminal: UpstreamAttemptTerminal,
) {
    match terminal {
        UpstreamAttemptTerminal::Succeeded => {
            if admission != UpstreamAttemptAdmission::Probe {
                return;
            }
            match state
                .db
                .record_upstream_account_success(upstream_account_id)
                .await
            {
                Ok(true) => state.metrics.observe_upstream_health(
                    UpstreamHealthEvent::Recovered,
                    UpstreamHealthReason::Success,
                ),
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    %request_id,
                    %upstream_account_id,
                    error = %error,
                    "failed to clear upstream account cooldown after a valid probe"
                ),
            }
        }
        UpstreamAttemptTerminal::Inconclusive => {
            if admission == UpstreamAttemptAdmission::Probe
                && let Err(error) = state
                    .db
                    .release_upstream_account_probe(upstream_account_id)
                    .await
            {
                tracing::warn!(
                    %request_id,
                    %upstream_account_id,
                    error = %error,
                    "failed to release inconclusive upstream probe"
                );
            }
        }
        UpstreamAttemptTerminal::Failed { kind, reason } => {
            if let Err(error) = state
                .db
                .record_upstream_account_failure(upstream_account_id, kind)
                .await
            {
                tracing::warn!(
                    %request_id,
                    %upstream_account_id,
                    error = %error,
                    "failed to persist unsuccessful upstream probe"
                );
            }
            state
                .metrics
                .observe_upstream_health(UpstreamHealthEvent::Failure, reason);
        }
    }
}
