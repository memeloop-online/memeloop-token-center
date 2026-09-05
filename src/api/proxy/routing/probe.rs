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
    credential_generation: i64,
    lease_token: Option<Uuid>,
    heartbeat_stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl UpstreamAttemptGuard {
    pub(super) fn new(
        state: &AppState,
        request_id: Uuid,
        upstream_account_id: Uuid,
        credential_generation: i64,
        admission: UpstreamAttemptAdmission,
    ) -> Self {
        debug_assert_ne!(admission, UpstreamAttemptAdmission::Unavailable);
        let lease_token = match admission {
            UpstreamAttemptAdmission::Probe { lease_token } => Some(lease_token),
            UpstreamAttemptAdmission::Healthy | UpstreamAttemptAdmission::Unavailable => None,
        };
        let heartbeat_stop = lease_token.map(|lease_token| {
            let (stop, mut stopped) = tokio::sync::oneshot::channel();
            let database = state.db.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = &mut stopped => break,
                        _ = tokio::time::sleep(std::time::Duration::from_millis(
                            crate::db::UPSTREAM_PROBE_HEARTBEAT_MILLIS,
                        )) => {
                            match database
                                .renew_upstream_account_probe(
                                    upstream_account_id,
                                    credential_generation,
                                    lease_token,
                                )
                                .await
                            {
                                Ok(true) => {}
                                Ok(false) => break,
                                Err(error) => {
                                    tracing::warn!(
                                        %request_id,
                                        %upstream_account_id,
                                        error = %error,
                                        "failed to renew upstream probe lease"
                                    );
                                    break;
                                }
                            }
                        }
                    }
                }
            });
            stop
        });
        Self {
            state: Some(state.clone()),
            request_id,
            upstream_account_id,
            credential_generation,
            lease_token,
            heartbeat_stop,
        }
    }

    pub(super) async fn complete(&mut self, terminal: UpstreamAttemptTerminal) {
        self.stop_heartbeat();
        let Some(state) = self.state.take() else {
            return;
        };
        record_terminal(
            state,
            self.request_id,
            self.upstream_account_id,
            self.credential_generation,
            self.lease_token,
            terminal,
        )
        .await;
    }

    fn stop_heartbeat(&mut self) {
        if let Some(stop) = self.heartbeat_stop.take() {
            let _ = stop.send(());
        }
    }
}

impl Drop for UpstreamAttemptGuard {
    fn drop(&mut self) {
        self.stop_heartbeat();
        let Some(state) = self.state.take() else {
            return;
        };
        let request_id = self.request_id;
        let upstream_account_id = self.upstream_account_id;
        let credential_generation = self.credential_generation;
        let lease_token = self.lease_token;
        // Proxy guards are created and dropped on the Tokio request runtime.
        // Cancellation cannot prove either upstream failure or recovery. It
        // must not make a half-open account healthy, and it must not let a
        // downstream disconnect poison an otherwise healthy account.
        tokio::spawn(async move {
            record_terminal(
                state,
                request_id,
                upstream_account_id,
                credential_generation,
                lease_token,
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
    credential_generation: i64,
    lease_token: Option<Uuid>,
    terminal: UpstreamAttemptTerminal,
) {
    match terminal {
        UpstreamAttemptTerminal::Succeeded => {
            let Some(lease_token) = lease_token else {
                return;
            };
            match state
                .db
                .record_upstream_account_probe_success(
                    upstream_account_id,
                    credential_generation,
                    lease_token,
                )
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
            if let Some(lease_token) = lease_token
                && let Err(error) = state
                    .db
                    .release_upstream_account_probe(
                        upstream_account_id,
                        credential_generation,
                        lease_token,
                    )
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
            let persisted = match lease_token {
                Some(lease_token) => {
                    state
                        .db
                        .record_upstream_account_probe_failure(
                            upstream_account_id,
                            credential_generation,
                            lease_token,
                            kind,
                        )
                        .await
                }
                None => {
                    state
                        .db
                        .record_upstream_account_failure(
                            upstream_account_id,
                            credential_generation,
                            kind,
                        )
                        .await
                }
            };
            match persisted {
                Ok(true) => state
                    .metrics
                    .observe_upstream_health(UpstreamHealthEvent::Failure, reason),
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    %request_id,
                    %upstream_account_id,
                    error = %error,
                    "failed to persist unsuccessful upstream attempt"
                ),
            }
        }
    }
}
