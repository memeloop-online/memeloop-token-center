use super::*;
use crate::metrics::{UpstreamHealthEvent, UpstreamHealthReason};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicU32, Ordering},
    },
};

type ProbeKey = (Uuid, i64);

struct SharedProbeLimiter {
    active: AtomicU32,
}

static SHARED_PROBE_LIMITERS: OnceLock<Mutex<HashMap<ProbeKey, Weak<SharedProbeLimiter>>>> =
    OnceLock::new();

pub(in crate::api::proxy) struct SharedProbePermit {
    limiter: Arc<SharedProbeLimiter>,
}

impl Drop for SharedProbePermit {
    fn drop(&mut self) {
        self.limiter.active.fetch_sub(1, Ordering::AcqRel);
    }
}

fn try_shared_probe_permit(
    upstream_account_id: Uuid,
    credential_generation: i64,
    limit: u32,
) -> Option<SharedProbePermit> {
    if limit == 0 {
        return None;
    }
    let limiters = SHARED_PROBE_LIMITERS.get_or_init(|| Mutex::new(HashMap::new()));
    let limiter = {
        let mut limiters = limiters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        limiters.retain(|_, limiter| limiter.strong_count() > 0);
        let key = (upstream_account_id, credential_generation);
        match limiters.get(&key).and_then(Weak::upgrade) {
            Some(limiter) => limiter,
            None => {
                let limiter = Arc::new(SharedProbeLimiter {
                    active: AtomicU32::new(0),
                });
                limiters.insert(key, Arc::downgrade(&limiter));
                limiter
            }
        }
    };
    let mut active = limiter.active.load(Ordering::Acquire);
    loop {
        if active >= limit {
            return None;
        }
        match limiter.active.compare_exchange_weak(
            active,
            active + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Some(SharedProbePermit { limiter }),
            Err(observed) => active = observed,
        }
    }
}

#[cfg(test)]
mod limiter_tests {
    use super::*;

    #[test]
    fn runtime_limit_changes_apply_without_revoking_in_flight_attempts() {
        let account = Uuid::new_v4();
        let permits = (0..4)
            .map(|_| try_shared_probe_permit(account, 1, 4).expect("capacity at limit four"))
            .collect::<Vec<_>>();
        assert!(try_shared_probe_permit(account, 1, 1).is_none());
        drop(permits);
        let lowered =
            try_shared_probe_permit(account, 1, 1).expect("lower limit applies after drain");
        assert!(try_shared_probe_permit(account, 1, 1).is_none());
        let raised =
            try_shared_probe_permit(account, 1, 4).expect("raised limit applies immediately");
        drop((lowered, raised));
    }
}

pub(in crate::api::proxy) async fn join_shared_probe(
    state: &AppState,
    upstream_account_id: Uuid,
    credential_generation: i64,
    shared_probe_attempts: u32,
) -> Result<Option<(UpstreamAttemptAdmission, SharedProbePermit)>, AppError> {
    let Some(permit) = try_shared_probe_permit(
        upstream_account_id,
        credential_generation,
        shared_probe_attempts,
    ) else {
        return Ok(None);
    };
    let Some(admission) = state
        .db
        .join_upstream_account_probe(upstream_account_id, credential_generation)
        .await?
    else {
        return Ok(None);
    };
    Ok(Some((admission, permit)))
}

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
    owns_probe_lease: bool,
    heartbeat_stop: Option<tokio::sync::oneshot::Sender<()>>,
    _shared_probe_permit: Option<SharedProbePermit>,
    delivery_recovery_attempted: bool,
    recovered_on_delivery: bool,
}

struct UpstreamAttemptRecord {
    state: AppState,
    request_id: Uuid,
    upstream_account_id: Uuid,
    credential_generation: i64,
    lease_token: Option<Uuid>,
    owns_probe_lease: bool,
    recovered_on_delivery: bool,
}

impl UpstreamAttemptGuard {
    pub(in crate::api::proxy) fn new(
        state: &AppState,
        request_id: Uuid,
        upstream_account_id: Uuid,
        credential_generation: i64,
        admission: UpstreamAttemptAdmission,
        shared_probe_permit: Option<SharedProbePermit>,
    ) -> Self {
        let lease_token = match admission {
            UpstreamAttemptAdmission::Probe { lease_token }
            | UpstreamAttemptAdmission::SharedProbe { lease_token } => Some(lease_token),
            UpstreamAttemptAdmission::Healthy => None,
            UpstreamAttemptAdmission::Unavailable { .. } => {
                debug_assert!(false, "unavailable upstream attempt cannot own a guard");
                None
            }
        };
        let owns_probe_lease = matches!(admission, UpstreamAttemptAdmission::Probe { .. });
        let heartbeat_stop = owns_probe_lease.then(|| {
            let lease_token = lease_token.expect("probe admission has a lease token");
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
            owns_probe_lease,
            heartbeat_stop,
            _shared_probe_permit: shared_probe_permit,
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
        let recovery = async {
            if self.owns_probe_lease {
                state
                    .db
                    .record_upstream_account_probe_delivery(
                        self.upstream_account_id,
                        self.credential_generation,
                        token,
                    )
                    .await
            } else {
                // A shared recovery request is independent evidence, not the
                // owner of the long-running probe epoch. Delete the health
                // row on its first validated delivery so a later owner failure
                // cannot overwrite this separate request's proven success.
                state
                    .db
                    .record_upstream_account_probe_success(
                        self.upstream_account_id,
                        self.credential_generation,
                        token,
                    )
                    .await
            }
        };
        match tokio::time::timeout(std::time::Duration::from_millis(250), recovery).await {
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
            UpstreamAttemptRecord {
                state,
                request_id: self.request_id,
                upstream_account_id: self.upstream_account_id,
                credential_generation: self.credential_generation,
                lease_token: self.lease_token,
                owns_probe_lease: self.owns_probe_lease,
                recovered_on_delivery: self.recovered_on_delivery,
            },
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
        let owns_probe_lease = self.owns_probe_lease;
        let recovered_on_delivery = self.recovered_on_delivery;
        // Proxy guards are created and dropped on the Tokio request runtime.
        // Cancellation cannot prove either upstream failure or recovery. It
        // must not make a half-open account healthy, and it must not let a
        // downstream disconnect poison an otherwise healthy account.
        tokio::spawn(async move {
            record_terminal(
                UpstreamAttemptRecord {
                    state,
                    request_id,
                    upstream_account_id,
                    credential_generation,
                    lease_token,
                    owns_probe_lease,
                    recovered_on_delivery,
                },
                UpstreamAttemptTerminal::Inconclusive,
            )
            .await;
        });
    }
}

async fn record_terminal(record: UpstreamAttemptRecord, terminal: UpstreamAttemptTerminal) {
    let UpstreamAttemptRecord {
        state,
        request_id,
        upstream_account_id,
        credential_generation,
        lease_token,
        owns_probe_lease,
        recovered_on_delivery,
    } = record;
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
            if owns_probe_lease
                && let Some(lease_token) = lease_token
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
            // A shared recovery request never owns the heartbeat lease. Its
            // connection/stream failure must not cancel the primary probe or
            // overwrite a concurrent success. A complete 429 is different:
            // it is authoritative capacity evidence and may fence the epoch
            // with the provider's bounded reset deadline.
            if lease_token.is_some()
                && !owns_probe_lease
                && !matches!(
                    kind,
                    UpstreamFailureKind::RateLimited | UpstreamFailureKind::RateLimitedUntil { .. }
                )
            {
                return;
            }
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
