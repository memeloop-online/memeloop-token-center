use super::*;
use crate::metrics::{UpstreamHealthEvent, UpstreamHealthReason};

#[derive(Clone, Copy)]
pub(in crate::api::proxy) enum UpstreamAttemptTerminal {
    Succeeded,
    Inconclusive,
    Failed {
        kind: UpstreamFailureKind,
        reason: UpstreamHealthReason,
    },
}

impl UpstreamAttemptTerminal {
    pub(in crate::api::proxy) const fn invalid_response() -> Self {
        Self::Failed {
            kind: UpstreamFailureKind::InvalidResponse,
            reason: UpstreamHealthReason::InvalidResponse,
        }
    }
}

/// Owns the half-open circuit-breaker transition after an HTTP response has
/// been admitted. A 2xx header is not recovery: only the buffered or streaming
/// terminal path can confirm final validity and settlement. Validated,
/// durably delivered streaming output may independently release half-open
/// admission without losing this attempt's fenced terminal responsibility.
#[must_use]
pub(in crate::api::proxy) struct UpstreamAttemptGuard {
    state: Option<AppState>,
    request_id: Uuid,
    upstream_account_id: Uuid,
    credential_generation: i64,
    lease_token: Option<Uuid>,
    heartbeat_stop: Option<tokio::sync::oneshot::Sender<()>>,
    delivery_recovery_attempted: bool,
    recovered_on_delivery: bool,
}

impl UpstreamAttemptGuard {
    pub(in crate::api::proxy) fn new(
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
            let upstream_health = state.config.upstream_health;
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = &mut stopped => break,
                        _ = tokio::time::sleep(crate::db::upstream_probe_heartbeat_interval(
                            upstream_health,
                        )) => {
                            match database
                                .renew_upstream_account_probe_with_health_config(
                                    upstream_account_id,
                                    credential_generation,
                                    lease_token,
                                    upstream_health,
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
            delivery_recovery_attempted: false,
            recovered_on_delivery: false,
        }
    }

    /// Call only after a protocol-validated billable frame is durably recorded
    /// and enqueued downstream. Headers, comments and usage-only frames do not
    /// establish recovery. This is not request completion or usage settlement.
    pub(in crate::api::proxy) async fn delivered_validated_output(&mut self) {
        if self.delivery_recovery_attempted {
            return;
        }
        self.delivery_recovery_attempted = true;
        let (Some(state), Some(token)) = (self.state.as_ref(), self.lease_token) else {
            return;
        };
        match tokio::time::timeout(
            std::time::Duration::from_millis(250),
            state.db.record_upstream_account_probe_delivery(
                self.upstream_account_id,
                self.credential_generation,
                token,
            ),
        )
        .await
        {
            Ok(Ok(true)) => {
                self.recovered_on_delivery = true;
                state.metrics.observe_upstream_health(
                    UpstreamHealthEvent::Recovered,
                    UpstreamHealthReason::Success,
                );
                self.stop_heartbeat();
            }
            Ok(Ok(false)) => {}
            _ => tracing::warn!(
                request_id = %self.request_id,
                upstream_account_id = %self.upstream_account_id,
                stage = "probe_delivery_ack",
                "streaming probe recovery acknowledgement unavailable"
            ),
        }
    }

    pub(in crate::api::proxy) async fn complete(&mut self, terminal: UpstreamAttemptTerminal) {
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
            self.recovered_on_delivery,
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
        let recovered_on_delivery = self.recovered_on_delivery;
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
                recovered_on_delivery,
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
    recovered_on_delivery: bool,
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
                Ok(true) if !recovered_on_delivery => state.metrics.observe_upstream_health(
                    UpstreamHealthEvent::Recovered,
                    UpstreamHealthReason::Success,
                ),
                Ok(_) => {}
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
                        .record_upstream_account_probe_failure_with_health_config(
                            upstream_account_id,
                            credential_generation,
                            lease_token,
                            kind,
                            state.config.upstream_health,
                        )
                        .await
                }
                None => {
                    state
                        .db
                        .record_upstream_account_failure_with_health_config(
                            upstream_account_id,
                            credential_generation,
                            kind,
                            state.config.upstream_health,
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
