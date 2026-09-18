use super::*;
#[cfg(test)]
mod observe_heartbeat_tests;
use crate::metrics::{UpstreamHealthEvent, UpstreamHealthReason};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicU32, Ordering},
    },
};

type ProbeKey = (Uuid, i64);

fn transient_sample_for_outcome(
    outcome: crate::plugin::routing::GroupRoutingOutcome,
) -> Option<bool> {
    use crate::plugin::routing::GroupRoutingOutcome;
    match outcome {
        GroupRoutingOutcome::Success => Some(false),
        GroupRoutingOutcome::TransientFailure => Some(true),
        GroupRoutingOutcome::HardQuota
        | GroupRoutingOutcome::Authentication
        | GroupRoutingOutcome::Cancelled => None,
    }
}

fn active_transient_keeps_breaker_closed(
    policy: crate::plugin::routing::GroupRoutingTransientPolicy,
    signal: crate::plugin::routing::GroupRoutingTransientSignal,
) -> bool {
    !signal.should_open(policy.min_samples, policy.open_micros)
}

fn active_transient_defers_probe_recovery(
    policy: crate::plugin::routing::GroupRoutingTransientPolicy,
    signal: crate::plugin::routing::GroupRoutingTransientSignal,
) -> bool {
    !signal.should_recover(policy.recover_micros, policy.min_probe_successes)
}

fn terminal_health_config(
    mut health: crate::config::UpstreamHealthConfig,
    directive: Option<&crate::plugin::routing::GroupRoutingExecutionDirective>,
    terminal: UpstreamAttemptTerminal,
) -> crate::config::UpstreamHealthConfig {
    if let Some(directive) = directive
        && directive.health_directives_enabled()
        && matches!(
            terminal,
            UpstreamAttemptTerminal::Failed {
                kind: UpstreamFailureKind::Connection
                    | UpstreamFailureKind::Unavailable
                    | UpstreamFailureKind::InvalidResponse,
                ..
            }
        )
    {
        // Only active-v2 and established v1 transient failures consume plugin
        // cooldown. Shadow v2 is observational and retains native health.
        let cooldown = directive.directive.cooldown_ms.min(60_000) as i64;
        health.connection_cooldown_millis = cooldown;
        health.unavailable_cooldown_millis = cooldown;
        health.invalid_response_cooldown_millis = cooldown;
    }
    health
}

struct SharedProbeLimiter {
    active: AtomicU32,
}

static SHARED_PROBE_LIMITERS: OnceLock<Mutex<HashMap<ProbeKey, Weak<SharedProbeLimiter>>>> =
    OnceLock::new();

#[derive(Clone, Debug)]
struct GatewayFailureDomainIdentity {
    pod: String,
    node: Option<String>,
    failure_domain: String,
}

fn bounded_identity_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty() && value.len() <= 253)
}

fn gateway_failure_domain_identity() -> GatewayFailureDomainIdentity {
    static IDENTITY: OnceLock<GatewayFailureDomainIdentity> = OnceLock::new();
    IDENTITY
        .get_or_init(|| {
            let pod = bounded_identity_value("MTC_GATEWAY_POD_NAME")
                .or_else(|| bounded_identity_value("HOSTNAME"))
                .unwrap_or_else(|| "unknown".to_owned());
            let node = bounded_identity_value("MTC_GATEWAY_NODE_NAME");
            let failure_domain = bounded_identity_value("MTC_GATEWAY_FAILURE_DOMAIN")
                .or_else(|| node.clone())
                // An unconfigured runtime intentionally forms one shared
                // domain: it cannot manufacture cross-domain evidence from
                // multiple processes on an unknown node.
                .unwrap_or_else(|| "unassigned".to_owned());
            GatewayFailureDomainIdentity {
                pod,
                node,
                failure_domain,
            }
        })
        .clone()
}

pub(crate) struct SharedProbePermit {
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

    fn execution_directive(
        mode: Option<crate::plugin::routing::GroupRoutingTransientPolicyMode>,
    ) -> crate::plugin::routing::GroupRoutingExecutionDirective {
        use crate::plugin::routing::{GroupRoutingDirective, GroupRoutingTransientPolicy};
        crate::plugin::routing::GroupRoutingExecutionDirective {
            directive: GroupRoutingDirective {
                tenant_id: Uuid::nil().to_string(),
                route_id: Uuid::from_u128(1).to_string(),
                account_id: Uuid::from_u128(2).to_string(),
                generation: 1,
                allow_transient_probe: true,
                cooldown_ms: 1,
                recovery_wait_ms: 1,
                recheck_ms: 1,
                stickiness: false,
            },
            transient_policy: mode.map(|mode| GroupRoutingTransientPolicy {
                mode,
                min_samples: 1,
                open_micros: 1,
                recover_micros: 1,
                min_probe_successes: 1,
            }),
        }
    }

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

    #[test]
    fn only_conclusive_transient_health_outcomes_are_samples() {
        use crate::plugin::routing::GroupRoutingOutcome;
        assert_eq!(
            transient_sample_for_outcome(GroupRoutingOutcome::Success),
            Some(false)
        );
        assert_eq!(
            transient_sample_for_outcome(GroupRoutingOutcome::TransientFailure),
            Some(true)
        );
        assert_eq!(
            transient_sample_for_outcome(GroupRoutingOutcome::Cancelled),
            None
        );
        assert_eq!(
            transient_sample_for_outcome(GroupRoutingOutcome::HardQuota),
            None
        );
        assert_eq!(
            transient_sample_for_outcome(GroupRoutingOutcome::Authentication),
            None
        );
    }

    #[test]
    fn shadow_v2_terminal_cooldown_retains_native_health_config() {
        use crate::{
            config::UpstreamHealthConfig, plugin::routing::GroupRoutingTransientPolicyMode,
        };
        let native = UpstreamHealthConfig::DEFAULT;
        let terminal = UpstreamAttemptTerminal::invalid_response();
        let shadow = execution_directive(Some(GroupRoutingTransientPolicyMode::Shadow));
        assert_eq!(
            terminal_health_config(native, Some(&shadow), terminal),
            native
        );

        let active = execution_directive(Some(GroupRoutingTransientPolicyMode::Active));
        let overridden = terminal_health_config(native, Some(&active), terminal);
        assert_eq!(overridden.connection_cooldown_millis, 1);
        assert_eq!(overridden.unavailable_cooldown_millis, 1);
        assert_eq!(overridden.invalid_response_cooldown_millis, 1);
        assert_eq!(
            overridden.rate_limited_cooldown_millis,
            native.rate_limited_cooldown_millis
        );

        let v1 = execution_directive(None);
        assert_eq!(
            terminal_health_config(native, Some(&v1), terminal),
            overridden,
            "v1 directive behavior remains unchanged"
        );
    }
}

#[cfg(test)]
mod v2_lifecycle_tests {
    use super::*;
    use crate::{
        config::UpstreamHealthConfig,
        db::{Database, UpstreamFailureKind, unix_millis},
        plugin::routing::{
            GroupRoutingOutcome, GroupRoutingTransientPolicy, GroupRoutingTransientPolicyMode,
            GroupRoutingTransientSignal,
        },
    };

    #[tokio::test]
    async fn active_v2_opens_and_recovers_only_at_threshold_without_replaying_or_sampling_cancelled()
     {
        let directory = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("v2-lifecycle.db").display()
        );
        let database = Database::connect(&url).await.unwrap();
        let pool = sqlx::AnyPool::connect(&url).await.unwrap();
        database.migrate().await.unwrap();
        let tenant = Uuid::now_v7();
        let account = Uuid::now_v7();
        sqlx::query("INSERT INTO tenants (id,external_id,created_at) VALUES ($1,$1,0)")
            .bind(tenant.to_string())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO upstream_accounts (id,tenant_id,name,driver,auth_kind,config_json,status,credential_generation,created_at,updated_at) VALUES ($1,$2,'v2 lifecycle','http-json','none','{}','active',1,0,0)")
            .bind(account.to_string())
            .bind(tenant.to_string())
            .execute(&pool)
            .await
            .unwrap();
        let policy = GroupRoutingTransientPolicy {
            mode: GroupRoutingTransientPolicyMode::Active,
            min_samples: 2,
            open_micros: 900_000,
            recover_micros: 600_000,
            min_probe_successes: 2,
        };
        let policy_scope = "a".repeat(64);
        macro_rules! plugin_signal {
            ($value:expr) => {{
                let signal = $value;
                GroupRoutingTransientSignal {
                    sample_count: signal.sample_count as u64,
                    ewma_micros: signal.ewma_micros as u32,
                    last_observed_at: signal.last_observed_at,
                    recovery_successes: signal.recovery_successes as u64,
                    revision: signal.revision as u64,
                }
            }};
        }

        let first = plugin_signal!(
            database
                .record_transient_health_sample_at(
                    account,
                    1,
                    &policy_scope,
                    true,
                    1_000,
                    crate::plugin::routing::DEFAULT_TRANSIENT_HEALTH_WINDOW_MS,
                )
                .await
                .unwrap()
                .unwrap()
        );
        assert!(active_transient_keeps_breaker_closed(policy, first));
        assert!(
            database
                .claim_upstream_account_attempt_with_health_config(
                    account,
                    1,
                    UpstreamHealthConfig::DEFAULT,
                )
                .await
                .unwrap()
                .is_healthy(),
            "the first failed request is not replayed and does not open before min_samples"
        );

        let second = plugin_signal!(
            database
                .record_transient_health_sample_at(
                    account,
                    1,
                    &policy_scope,
                    true,
                    1_001,
                    crate::plugin::routing::DEFAULT_TRANSIENT_HEALTH_WINDOW_MS,
                )
                .await
                .unwrap()
                .unwrap()
        );
        assert!(!active_transient_keeps_breaker_closed(policy, second));
        database
            .record_upstream_account_failure(account, 1, UpstreamFailureKind::Connection)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE upstream_account_health SET cooldown_until = 0 WHERE upstream_account_id = $1",
        )
        .bind(account.to_string())
        .execute(&pool)
        .await
        .unwrap();
        let first_probe = database
            .claim_upstream_account_attempt_with_health_config(
                account,
                1,
                UpstreamHealthConfig::DEFAULT,
            )
            .await
            .unwrap();
        let UpstreamAttemptAdmission::Probe {
            lease_token: first_lease,
        } = first_probe
        else {
            panic!("threshold crossing must admit exactly one probe")
        };
        let first_success = plugin_signal!(
            database
                .record_transient_health_sample_at(
                    account,
                    1,
                    &policy_scope,
                    false,
                    1_002,
                    crate::plugin::routing::DEFAULT_TRANSIENT_HEALTH_WINDOW_MS,
                )
                .await
                .unwrap()
                .unwrap()
        );
        assert!(active_transient_defers_probe_recovery(
            policy,
            first_success
        ));
        assert!(
            database
                .defer_upstream_account_probe_recovery(account, 1, first_lease, 0)
                .await
                .unwrap()
        );
        let second_probe = database
            .claim_upstream_account_attempt_with_health_config(
                account,
                1,
                UpstreamHealthConfig::DEFAULT,
            )
            .await
            .unwrap();
        let UpstreamAttemptAdmission::Probe {
            lease_token: second_lease,
        } = second_probe
        else {
            panic!("deferred recovery must retain half-open state")
        };
        let second_success = plugin_signal!(
            database
                .record_transient_health_sample_at(
                    account,
                    1,
                    &policy_scope,
                    false,
                    1_003,
                    crate::plugin::routing::DEFAULT_TRANSIENT_HEALTH_WINDOW_MS,
                )
                .await
                .unwrap()
                .unwrap()
        );
        assert!(!active_transient_defers_probe_recovery(
            policy,
            second_success
        ));
        assert!(
            database
                .record_upstream_account_probe_success(account, 1, second_lease)
                .await
                .unwrap()
        );

        let before_cancel: i64 = sqlx::query_scalar(
            "SELECT revision FROM group_routing_v2_transient_health_signals WHERE upstream_account_id = $1 AND policy_scope = $2",
        )
        .bind(account.to_string())
        .bind(&policy_scope)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            transient_sample_for_outcome(GroupRoutingOutcome::Cancelled),
            None
        );
        let after_cancel: i64 = sqlx::query_scalar(
            "SELECT revision FROM group_routing_v2_transient_health_signals WHERE upstream_account_id = $1 AND policy_scope = $2",
        )
        .bind(account.to_string())
        .bind(&policy_scope)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(after_cancel, before_cancel);
        assert!(
            database
                .claim_upstream_account_attempt_with_health_config(
                    account,
                    1,
                    UpstreamHealthConfig::DEFAULT,
                )
                .await
                .unwrap()
                .is_healthy()
        );
        assert!(unix_millis() >= second_success.last_observed_at);
        pool.close().await;
    }
}

pub(in crate::api::proxy) async fn join_shared_probe(
    state: &AppState,
    upstream_account_id: Uuid,
    credential_generation: i64,
    transport_revision: i64,
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
        .join_upstream_account_probe_at_revision(
            upstream_account_id,
            credential_generation,
            transport_revision,
        )
        .await?
    else {
        return Ok(None);
    };
    Ok(Some((admission, permit)))
}

#[derive(Clone, Copy)]
pub(crate) enum UpstreamAttemptTerminal {
    Succeeded,
    Inconclusive,
    Failed {
        kind: UpstreamFailureKind,
        reason: UpstreamHealthReason,
        failure_stage: &'static str,
    },
}

impl UpstreamAttemptTerminal {
    pub(crate) const fn invalid_response() -> Self {
        Self::Failed {
            kind: UpstreamFailureKind::InvalidResponse,
            reason: UpstreamHealthReason::InvalidResponse,
            failure_stage: "response_validation",
        }
    }
}

/// Owns the half-open circuit-breaker transition after an HTTP response has
/// been admitted. A 2xx header is not recovery: only the buffered or streaming
/// terminal path can confirm final validity and settlement. Validated,
/// durably delivered streaming output may independently release half-open
/// admission without losing this attempt's fenced terminal responsibility.
#[must_use]
pub(crate) struct UpstreamAttemptGuard {
    state: Option<AppState>,
    request_id: Uuid,
    route_id: Uuid,
    upstream_account_id: Uuid,
    credential_generation: i64,
    transport_revision: i64,
    gateway_identity: GatewayFailureDomainIdentity,
    failure_epoch: Option<Uuid>,
    lease_token: Option<Uuid>,
    owns_probe_lease: bool,
    heartbeat_stop: Option<tokio::sync::oneshot::Sender<()>>,
    _shared_probe_permit: Option<SharedProbePermit>,
    delivery_recovery_attempted: bool,
    recovered_on_delivery: bool,
    defer_delivery_recovery: bool,
}

struct UpstreamAttemptRecord {
    state: AppState,
    request_id: Uuid,
    route_id: Uuid,
    upstream_account_id: Uuid,
    credential_generation: i64,
    transport_revision: i64,
    gateway_identity: GatewayFailureDomainIdentity,
    failure_epoch: Option<Uuid>,
    lease_token: Option<Uuid>,
    owns_probe_lease: bool,
    recovered_on_delivery: bool,
}

impl UpstreamAttemptGuard {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        state: &AppState,
        request_id: Uuid,
        route_id: Uuid,
        upstream_account_id: Uuid,
        credential_generation: i64,
        transport_revision: i64,
        admission: UpstreamAttemptAdmission,
        shared_probe_permit: Option<SharedProbePermit>,
    ) -> Self {
        let (lease_token, failure_epoch) = match admission {
            UpstreamAttemptAdmission::Probe { lease_token }
            | UpstreamAttemptAdmission::SharedProbe { lease_token } => (Some(lease_token), None),
            UpstreamAttemptAdmission::Healthy { failure_epoch } => (None, Some(failure_epoch)),
            UpstreamAttemptAdmission::Unavailable { .. } => {
                debug_assert!(false, "unavailable upstream attempt cannot own a guard");
                (None, None)
            }
        };
        let owns_probe_lease = matches!(admission, UpstreamAttemptAdmission::Probe { .. });
        let defer_delivery_recovery = state.group_routing.as_ref().is_some_and(|snapshot| {
            snapshot
                .active_transient_policy(route_id, upstream_account_id, credential_generation)
                .is_some()
        });
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
                                .renew_upstream_account_probe_at_revision_with_health_config(
                                    upstream_account_id,
                                    credential_generation,
                                    transport_revision,
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
                                        error_category = error.diagnostic_category(),
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
            route_id,
            upstream_account_id,
            credential_generation,
            transport_revision,
            gateway_identity: gateway_failure_domain_identity(),
            failure_epoch,
            lease_token,
            owns_probe_lease,
            heartbeat_stop,
            _shared_probe_permit: shared_probe_permit,
            delivery_recovery_attempted: false,
            recovered_on_delivery: false,
            defer_delivery_recovery,
        }
    }

    pub(crate) const fn route_assignment(&self) -> (Uuid, Uuid) {
        (self.route_id, self.upstream_account_id)
    }

    /// Call only after a protocol-validated billable frame is durably recorded
    /// and enqueued downstream. Headers, comments and usage-only frames do not
    /// establish recovery. This is not request completion or usage settlement.
    pub(in crate::api::proxy) async fn delivered_validated_output(&mut self) {
        if self.delivery_recovery_attempted || self.defer_delivery_recovery {
            return;
        }
        self.delivery_recovery_attempted = true;
        let Some(state) = self.state.as_ref() else {
            return;
        };
        let recovery = async {
            if let Some(token) = self.lease_token
                && self.owns_probe_lease
            {
                state
                    .db
                    .record_upstream_account_probe_delivery_at_revision(
                        self.upstream_account_id,
                        self.credential_generation,
                        self.transport_revision,
                        token,
                    )
                    .await
            } else if let Some(token) = self.lease_token {
                // A shared recovery request is independent evidence, not the
                // owner of the long-running probe epoch. Rotate the lease into
                // a healthy admission epoch on its first validated delivery so
                // a later owner failure cannot overwrite this proven success.
                state
                    .db
                    .record_upstream_account_probe_success_at_revision(
                        self.upstream_account_id,
                        self.credential_generation,
                        self.transport_revision,
                        token,
                    )
                    .await
            } else {
                // Healthy admissions keep their cohort fence until complete
                // protocol and usage validation. Partial delivery can still
                // end in a terminal invalid response, which must retain the
                // original epoch so that failure remains authoritative.
                Ok(false)
            }
        };
        match tokio::time::timeout(std::time::Duration::from_millis(250), recovery).await {
            Ok(Ok(true)) if self.lease_token.is_some() => {
                self.recovered_on_delivery = true;
                state.metrics.observe_upstream_health(
                    UpstreamHealthEvent::Recovered,
                    UpstreamHealthReason::Success,
                );
                self.stop_heartbeat();
            }
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => {}
            _ => tracing::warn!(
                request_id = %self.request_id,
                upstream_account_id = %self.upstream_account_id,
                stage = "probe_delivery_ack",
                "streaming probe recovery acknowledgement unavailable"
            ),
        }
    }

    pub(crate) async fn complete(&mut self, terminal: UpstreamAttemptTerminal) {
        let Some(state) = self.state.take() else {
            return;
        };
        // Observe may wait for bounded component capacity. Keep renewing the
        // owner fence until its conclusive database transition has finished;
        // short probe leases must not expire during plugin observation.
        record_terminal(
            UpstreamAttemptRecord {
                state,
                request_id: self.request_id,
                route_id: self.route_id,
                upstream_account_id: self.upstream_account_id,
                credential_generation: self.credential_generation,
                transport_revision: self.transport_revision,
                gateway_identity: self.gateway_identity.clone(),
                failure_epoch: self.failure_epoch,
                lease_token: self.lease_token,
                owns_probe_lease: self.owns_probe_lease,
                recovered_on_delivery: self.recovered_on_delivery,
            },
            terminal,
        )
        .await;
        self.stop_heartbeat();
    }

    /// A successful durable CAS has ended the caller's ownership lease. Keep
    /// the already-proven terminal transition and its heartbeat alive even if
    /// that caller's timeout/cancellation drops this waiting future. The owned
    /// task performs only bounded observation and fenced health persistence;
    /// it has no authority to send or replay an upstream request.
    pub(crate) async fn complete_committed(mut self, terminal: UpstreamAttemptTerminal) {
        let completion = tokio::spawn(async move { self.complete(terminal).await });
        if completion.await.is_err() {
            tracing::warn!(
                stage = "committed_health_completion",
                "committed upstream health completion task failed"
            );
        }
    }

    /// A lost durable job fence grants no authority to publish an observation.
    /// Release only this exact owned lease; never heal or record a failure.
    pub(crate) async fn abandon_without_observe(&mut self) {
        let state = self.state.take();
        self.stop_heartbeat();
        if let Some(state) = state
            && self.owns_probe_lease
            && !self.recovered_on_delivery
            && let Some(token) = self.lease_token
        {
            let _ = state
                .db
                .release_upstream_account_probe_at_revision(
                    self.upstream_account_id,
                    self.credential_generation,
                    self.transport_revision,
                    token,
                )
                .await;
        }
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
        let route_id = self.route_id;
        let upstream_account_id = self.upstream_account_id;
        let credential_generation = self.credential_generation;
        let transport_revision = self.transport_revision;
        let gateway_identity = self.gateway_identity.clone();
        let failure_epoch = self.failure_epoch;
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
                    route_id,
                    upstream_account_id,
                    credential_generation,
                    transport_revision,
                    gateway_identity,
                    failure_epoch,
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
        route_id,
        upstream_account_id,
        credential_generation,
        transport_revision,
        gateway_identity,
        failure_epoch,
        lease_token,
        owns_probe_lease,
        recovered_on_delivery,
    } = record;
    use crate::plugin::routing::GroupRoutingOutcome;
    let outcome = match terminal {
        UpstreamAttemptTerminal::Succeeded => GroupRoutingOutcome::Success,
        UpstreamAttemptTerminal::Inconclusive => GroupRoutingOutcome::Cancelled,
        UpstreamAttemptTerminal::Failed {
            kind: UpstreamFailureKind::RateLimited | UpstreamFailureKind::RateLimitedUntil { .. },
            ..
        } => GroupRoutingOutcome::HardQuota,
        UpstreamAttemptTerminal::Failed {
            kind: UpstreamFailureKind::Authentication,
            ..
        } => GroupRoutingOutcome::Authentication,
        UpstreamAttemptTerminal::Failed { .. } => GroupRoutingOutcome::TransientFailure,
    };
    let transient_health_scope = state.group_routing.as_ref().and_then(|snapshot| {
        snapshot
            .transient_health_scope(route_id, upstream_account_id, credential_generation)
            .cloned()
    });
    let signal = if let Some(transient_failure) = transient_sample_for_outcome(outcome)
        && let Some(scope) = transient_health_scope
    {
        match state
            .db
            .record_transient_health_sample(
                upstream_account_id,
                credential_generation,
                &scope.id,
                transient_failure,
                scope.window_ms,
            )
            .await
        {
            Ok(signal) => signal.map(|signal| {
                signal.for_group_routing_window(crate::db::unix_millis(), scope.window_ms)
            }),
            Err(error) => {
                tracing::warn!(%request_id, %upstream_account_id, error_category=error.diagnostic_category(), stage="transient_health_sample", "transient health signal unavailable; native health policy retained");
                None
            }
        }
    } else {
        None
    };
    let directive = crate::group_routing::observe_with_signal(
        &state,
        request_id,
        route_id,
        upstream_account_id,
        credential_generation,
        outcome,
        signal,
    )
    .await;
    let health = terminal_health_config(state.config.upstream_health, directive.as_ref(), terminal);
    match terminal {
        UpstreamAttemptTerminal::Succeeded => {
            if let Err(error) = state
                .db
                .clear_upstream_connection_failure_domain(
                    upstream_account_id,
                    credential_generation,
                    transport_revision,
                    &gateway_identity.failure_domain,
                )
                .await
            {
                tracing::warn!(%request_id, %upstream_account_id, error_category=error.diagnostic_category(), stage="connection_failure_domain_recovery", "failed to clear recovered gateway failure domain");
            }
            if let (Some(policy), Some(signal), Some(lease_token)) = (
                directive
                    .as_ref()
                    .and_then(|entry| entry.transient_policy)
                    .filter(|policy| {
                        policy.is_active()
                            && state.group_routing.as_ref().is_some_and(|snapshot| {
                                snapshot.active_transient_policy(
                                    route_id,
                                    upstream_account_id,
                                    credential_generation,
                                ) == Some(*policy)
                            })
                    }),
                signal,
                lease_token,
            ) && active_transient_defers_probe_recovery(policy, signal)
            {
                if owns_probe_lease && !recovered_on_delivery {
                    let cooldown = directive.as_ref().map_or(
                        state.config.upstream_health.connection_cooldown_millis,
                        |entry| entry.directive.cooldown_ms.min(60_000) as i64,
                    );
                    if let Err(error) = state
                        .db
                        .defer_upstream_account_probe_recovery_at_revision(
                            upstream_account_id,
                            credential_generation,
                            transport_revision,
                            lease_token,
                            cooldown.max(0) as u64,
                        )
                        .await
                    {
                        tracing::warn!(%request_id, %upstream_account_id, error_category=error.diagnostic_category(), "failed to defer transient probe recovery");
                    }
                }
                return;
            }
            let recovery = async {
                match lease_token {
                    Some(lease_token) => {
                        state
                            .db
                            .record_upstream_account_probe_success_at_revision(
                                upstream_account_id,
                                credential_generation,
                                transport_revision,
                                lease_token,
                            )
                            .await
                    }
                    _ => {
                        let Some(failure_epoch) = failure_epoch else {
                            return Ok(false);
                        };
                        state
                            .db
                            .record_upstream_account_success_at_revision(
                                upstream_account_id,
                                credential_generation,
                                transport_revision,
                                failure_epoch,
                            )
                            .await
                    }
                }
            };
            match recovery.await {
                Ok(true) if lease_token.is_some() && !recovered_on_delivery => {
                    state.metrics.observe_upstream_health(
                        UpstreamHealthEvent::Recovered,
                        UpstreamHealthReason::Success,
                    )
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(
                    %request_id,
                    %upstream_account_id,
                    error_category = error.diagnostic_category(),
                    "failed to clear upstream account cooldown after a valid probe"
                ),
            }
        }
        UpstreamAttemptTerminal::Inconclusive => {
            if owns_probe_lease
                && !recovered_on_delivery
                && let Some(lease_token) = lease_token
                && let Err(error) = state
                    .db
                    .release_upstream_account_probe_at_revision(
                        upstream_account_id,
                        credential_generation,
                        transport_revision,
                        lease_token,
                    )
                    .await
            {
                tracing::warn!(
                    %request_id,
                    %upstream_account_id,
                    error_category = error.diagnostic_category(),
                    "failed to release inconclusive upstream probe"
                );
            }
        }
        UpstreamAttemptTerminal::Failed {
            kind,
            reason,
            failure_stage,
        } => {
            if let Err(error) = state
                .db
                .record_upstream_transport_diagnostic(crate::db::UpstreamTransportDiagnostic {
                    request_id,
                    route_id,
                    upstream_account_id,
                    credential_generation,
                    transport_revision,
                    failure_kind: kind.as_str(),
                    failure_stage,
                    gateway_pod: &gateway_identity.pod,
                    gateway_node: gateway_identity.node.as_deref(),
                    failure_domain: &gateway_identity.failure_domain,
                })
                .await
            {
                tracing::warn!(%request_id, %upstream_account_id, error_category=error.diagnostic_category(), stage="transport_diagnostic_persist", "failed to persist upstream transport diagnostic");
            }
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
            if lease_token.is_none()
                && matches!(
                    kind,
                    UpstreamFailureKind::Connection
                        | UpstreamFailureKind::Unavailable
                        | UpstreamFailureKind::InvalidResponse
                )
                && let (Some(policy), Some(signal)) = (
                    directive
                        .as_ref()
                        .and_then(|entry| entry.transient_policy)
                        .filter(|policy| policy.is_active()),
                    signal,
                )
                && active_transient_keeps_breaker_closed(policy, signal)
            {
                // The conclusive sample is durable, but an explicitly active
                // v2 policy has not accumulated enough evidence to open. This
                // request is never replayed; a later request receives the
                // next frozen policy snapshot.
                return;
            }
            let persisted = match lease_token {
                Some(lease_token) => {
                    state
                        .db
                        .record_upstream_account_probe_failure_at_revision_with_health_config(
                            upstream_account_id,
                            credential_generation,
                            transport_revision,
                            lease_token,
                            kind,
                            health,
                        )
                        .await
                }
                None => match failure_epoch {
                    Some(failure_epoch) if kind == UpstreamFailureKind::Connection => state
                        .db
                        .record_admitted_connection_failure_by_domain(
                            crate::db::AdmittedConnectionFailure {
                                request_id,
                                upstream_account_id,
                                credential_generation,
                                transport_revision,
                                failure_epoch,
                                failure_stage,
                                gateway_pod: &gateway_identity.pod,
                                gateway_node: gateway_identity.node.as_deref(),
                                failure_domain: &gateway_identity.failure_domain,
                            },
                            health,
                        )
                        .await
                        .map(|result| {
                            tracing::warn!(
                                %request_id,
                                %upstream_account_id,
                                failure_domain = %gateway_identity.failure_domain,
                                distinct_failure_domains = result.distinct_failure_domains,
                                global_breaker_opened = result.global_breaker_opened,
                                stage = "connection_failure_domain_health",
                                "recorded failure-domain connection evidence"
                            );
                            result.global_breaker_opened
                        }),
                    Some(failure_epoch)
                        if matches!(
                            kind,
                            UpstreamFailureKind::Connection
                                | UpstreamFailureKind::Unavailable
                                | UpstreamFailureKind::InvalidResponse
                        ) =>
                    {
                        state
                            .db
                            .record_admitted_upstream_account_failure_at_revision(
                                upstream_account_id,
                                credential_generation,
                                transport_revision,
                                kind,
                                health,
                                failure_epoch,
                            )
                            .await
                    }
                    _ => {
                        state
                            .db
                            .record_upstream_account_failure_at_revision_with_health_config(
                                upstream_account_id,
                                credential_generation,
                                transport_revision,
                                kind,
                                health,
                            )
                            .await
                    }
                },
            };
            match persisted {
                Ok(true) => state
                    .metrics
                    .observe_upstream_health(UpstreamHealthEvent::Failure, reason),
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    %request_id,
                    %upstream_account_id,
                    error_category = error.diagnostic_category(),
                    "failed to persist unsuccessful upstream attempt"
                ),
            }
        }
    }
}
