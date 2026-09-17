//! Request-owned group policy. Authorization and health leases stay in core.
pub(crate) mod durable;
mod quota;
#[cfg(test)]
pub(crate) mod test_observe_gate;
use crate::{
    AppState,
    error::AppError,
    plugin::routing::{
        GROUP_ROUTING_V2_VERSION, GroupRoutingCandidate, GroupRoutingDirective, GroupRoutingHealth,
        GroupRoutingInput, GroupRoutingObserveInput, GroupRoutingOutcome,
        GroupRoutingTransientPolicy, GroupRoutingTransientSignal,
    },
    provider::AuthorizedUpstreamCandidate,
};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use uuid::Uuid;

type BucketMember = (
    usize,
    AuthorizedUpstreamCandidate,
    crate::db::GroupRoutingStrategy,
    i64,
);
type StrategyBuckets = BTreeMap<(std::cmp::Reverse<i32>, String), Vec<BucketMember>>;
type CandidateKey = (Uuid, Uuid, i64);

fn is_false(value: &bool) -> bool {
    !*value
}

/// The native/no-hook entrance is synchronous: do not add an unbounded
/// configuration query before the core scheduling deadline has been frozen.
pub(crate) fn candidate_snapshot_if_enabled(
    state: &AppState,
    candidates: &[AuthorizedUpstreamCandidate],
) -> Option<Vec<AuthorizedUpstreamCandidate>> {
    state
        .plugins
        .has_group_routing_hooks()
        .then(|| candidates.to_vec())
}

fn planned_candidate<'a>(
    input: &'a GroupRoutingInput,
    directive: &GroupRoutingDirective,
) -> Option<&'a GroupRoutingCandidate> {
    input.candidates.iter().find(|candidate| {
        candidate.tenant_id == directive.tenant_id
            && candidate.route_id == directive.route_id
            && candidate.account_id == directive.account_id
            && candidate.generation == directive.generation
    })
}

fn reserve_native_bucket_ranks(
    members: &[BucketMember],
    ranks: &mut BTreeMap<CandidateKey, usize>,
    next_rank: &mut usize,
) -> usize {
    let start = *next_rank;
    for (_, candidate, _, _) in members {
        ranks.insert(
            (
                candidate.route_id,
                candidate.account_id,
                candidate.credential_generation,
            ),
            *next_rank,
        );
        *next_rank += 1;
    }
    start
}

fn sort_plan_candidates(selection_seed: Uuid, directives: &mut [GroupRoutingDirective]) {
    // Stable sort preserves plugin order among non-sticky candidates.
    directives.sort_by_key(|directive| {
        let rank = if directive.stickiness {
            let mut hasher = blake3::Hasher::new();
            hasher.update(selection_seed.as_bytes());
            hasher.update(directive.route_id.as_bytes());
            hasher.update(directive.account_id.as_bytes());
            u64::from_le_bytes(hasher.finalize().as_bytes()[..8].try_into().unwrap())
        } else {
            0
        };
        (!directive.stickiness, rank)
    });
}

fn sort_execution_candidates(
    selection_seed: Uuid,
    directives: &mut [crate::plugin::routing::GroupRoutingExecutionDirective],
) {
    directives.sort_by_key(|entry| {
        let directive = &entry.directive;
        let rank = if directive.stickiness {
            let mut hasher = blake3::Hasher::new();
            hasher.update(selection_seed.as_bytes());
            hasher.update(directive.route_id.as_bytes());
            hasher.update(directive.account_id.as_bytes());
            u64::from_le_bytes(hasher.finalize().as_bytes()[..8].try_into().unwrap())
        } else {
            0
        };
        (!directive.stickiness, rank)
    });
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CandidatePolicy {
    plugin_id: String,
    group_id: String,
    strategy_version: i64,
    config: serde_json::Value,
    candidate: GroupRoutingCandidate,
    directive: GroupRoutingDirective,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transient_policy: Option<GroupRoutingTransientPolicy>,
    #[serde(default, skip_serializing_if = "is_false")]
    transient_signal_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transient_signal: Option<GroupRoutingTransientSignal>,
}

pub(crate) struct RequestGroupRouting {
    pub tenant_id: Uuid,
    seed: u64,
    started: tokio::time::Instant,
    deadline: tokio::time::Instant,
    policies: BTreeMap<(Uuid, Uuid, i64), CandidatePolicy>,
}

impl RequestGroupRouting {
    pub(crate) fn policy(
        &self,
        route: Uuid,
        account: Uuid,
        generation: i64,
    ) -> Option<&CandidatePolicy> {
        self.policies.get(&(route, account, generation))
    }

    pub(crate) fn uses_transient_signal(
        &self,
        route: Uuid,
        account: Uuid,
        generation: i64,
    ) -> bool {
        self.policy(route, account, generation)
            .is_some_and(|policy| policy.transient_signal_enabled)
    }

    pub(crate) fn active_transient_policy(
        &self,
        route: Uuid,
        account: Uuid,
        generation: i64,
    ) -> Option<GroupRoutingTransientPolicy> {
        self.policy(route, account, generation)
            .and_then(CandidatePolicy::active_transient_policy)
    }
}

impl CandidatePolicy {
    fn has_valid_transient_snapshot(&self) -> bool {
        if self.transient_signal_enabled {
            return self.transient_signal.is_some()
                && self
                    .transient_policy
                    .is_some_and(|policy| policy.is_valid());
        }
        self.transient_signal.is_none() && self.transient_policy.is_none()
    }

    pub(crate) fn allow_probe(&self) -> bool {
        self.directive.allow_transient_probe
    }
    pub(crate) fn cooldown_ms(&self) -> u64 {
        self.directive.cooldown_ms
    }
    pub(crate) fn recheck(&self) -> Duration {
        Duration::from_millis(self.directive.recheck_ms.clamp(25, 5_000))
    }
    pub(crate) fn wait_deadline(
        &self,
        snapshot: &RequestGroupRouting,
        core: tokio::time::Instant,
    ) -> tokio::time::Instant {
        core.min(snapshot.started + Duration::from_millis(self.directive.recovery_wait_ms))
    }
    pub(crate) fn active_transient_policy(&self) -> Option<GroupRoutingTransientPolicy> {
        self.transient_policy.filter(|policy| policy.is_active())
    }
}

/// Never repin application plugins here: state.plugins is the request's fixed
/// application revision, and survives into streaming terminal guards.
pub(crate) async fn prepare(
    state: &mut AppState,
    tenant_id: Uuid,
    selection_seed: Uuid,
    request_id: Uuid,
    deadline: tokio::time::Instant,
    candidates: &mut [AuthorizedUpstreamCandidate],
) -> Result<(), AppError> {
    let stage_deadline = deadline.min(tokio::time::Instant::now() + Duration::from_millis(250));
    match tokio::time::timeout_at(
        stage_deadline,
        prepare_inner(
            state,
            tenant_id,
            selection_seed,
            request_id,
            deadline,
            candidates,
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(%request_id, stage="group_routing_native_fallback", "group snapshot or execution exceeded request scheduling budget");
            Ok(())
        }
    }
}

async fn prepare_inner(
    state: &mut AppState,
    tenant_id: Uuid,
    selection_seed: Uuid,
    request_id: Uuid,
    deadline: tokio::time::Instant,
    candidates: &mut [AuthorizedUpstreamCandidate],
) -> Result<(), AppError> {
    // This runs inside the whole-stage deadline, and avoids the larger
    // candidate/group/health join for tenants retaining native scheduling.
    if !state.db.has_group_routing_strategies(tenant_id).await? {
        return Ok(());
    }
    let started = tokio::time::Instant::now();
    // Many overlapping groups must not multiply per-component execution into
    // an unbounded request stall. Only one blocking hook is ever outstanding.
    let hook_deadline = deadline.min(started + Duration::from_millis(250));
    let remaining_deadline_ms = deadline.saturating_duration_since(started).as_millis() as u64;
    let seed = u64::from_le_bytes(selection_seed.as_bytes()[..8].try_into().unwrap());
    let mut buckets = StrategyBuckets::new();
    if candidates.len() > 1024 {
        tracing::warn!(%request_id, stage="group_routing_native_fallback", "candidate set exceeds bounded group protocol");
        return Ok(());
    }
    let candidate_ids = candidates
        .iter()
        .map(|candidate| {
            (
                candidate.route_id,
                candidate.account_id,
                candidate.credential_generation,
            )
        })
        .collect::<Vec<_>>();
    let bindings = state
        .db
        .candidate_group_strategies(tenant_id, &candidate_ids)
        .await?
        .into_iter()
        .map(|(route, account, binding)| ((route, account, binding.generation), binding))
        .collect::<BTreeMap<_, _>>();
    if bindings.is_empty() {
        return Ok(());
    }
    for (index, candidate) in candidates.iter().enumerate() {
        if let Some(binding) = bindings.get(&(
            candidate.route_id,
            candidate.account_id,
            candidate.credential_generation,
        )) {
            buckets
                .entry((std::cmp::Reverse(binding.priority), binding.id.clone()))
                .or_default()
                .push((
                    index,
                    candidate.clone(),
                    binding.strategy.clone(),
                    binding.version,
                ));
        }
    }
    // Only selected manifest-opted-in buckets need quota data. One bounded
    // shared-store batch, never a supplier read on the scheduling path.
    let quota_plugins = state.plugins.quota_observation_plugin_ids();
    let quota_candidates: Vec<_> = buckets
        .values()
        .flatten()
        .filter(|member| quota_plugins.contains(&member.2.plugin_id))
        .map(|member| {
            (
                (
                    member.1.account_id,
                    member.1.credential_generation,
                    member.1.transport_revision,
                ),
                member.1.clone(),
            )
        })
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect();
    let quota_observations = if quota_candidates.is_empty() {
        BTreeMap::new()
    } else {
        match tokio::time::timeout_at(
            hook_deadline,
            crate::upstream_quota::observations::routing_observations(
                state,
                tenant_id,
                &quota_candidates,
                crate::db::unix_millis(),
            ),
        )
        .await
        {
            Ok(Ok(observations)) => observations,
            Ok(Err(error)) => {
                tracing::warn!(%request_id, stage="group_routing_quota_fallback", reason="read_failed", error_category=error.diagnostic_category(), accounts=quota_candidates.len(), "quota observation unavailable; native order retained");
                BTreeMap::new()
            }
            Err(_) => {
                tracing::warn!(%request_id, stage="group_routing_quota_fallback", reason="deadline", accounts=quota_candidates.len(), "quota observation unavailable; native order retained");
                BTreeMap::new()
            }
        }
    };
    let quota_now_ms = crate::db::unix_millis();
    let mut policies = BTreeMap::new();
    let mut ranks = BTreeMap::new();
    let mut next_rank = 0usize;
    for ((_, group_id), members) in buckets {
        // A broken strategy does not demote its high-priority group behind
        // healthy lower-priority plugins. Reserve native order first, and
        // replace only this bucket's slots after a validated plan succeeds.
        let bucket_rank = reserve_native_bucket_ranks(&members, &mut ranks, &mut next_rank);
        if tokio::time::Instant::now() >= hook_deadline {
            tracing::warn!(%request_id, %group_id, stage="group_routing_native_fallback", "request group scheduling budget exhausted");
            continue;
        }
        if members.iter().any(|member| member.3 != members[0].3) {
            tracing::warn!(%request_id, %group_id, stage="group_routing_native_fallback", "group changed while preparing snapshot; native policy retained");
            continue;
        }
        let config = members[0].2.config.clone();
        let plugin_id = members[0].2.plugin_id.clone();
        let plugin_is_v2 =
            state.plugins.group_routing_version(&plugin_id) == Some(GROUP_ROUTING_V2_VERSION);
        let native_health = state.plugins.group_routing_uses_native_health(&plugin_id);
        let quota_context = if state.plugins.group_routing_uses_quota_context(&plugin_id) {
            match quota::context_for_bucket(&members, &quota_observations, quota_now_ms) {
                Some(context) => Some(context),
                None => {
                    tracing::debug!(%request_id, %group_id, stage="group_routing_quota_fallback", reason="missing_or_stale", "quota evidence incomplete; native group order retained");
                    continue;
                }
            }
        } else {
            None
        };
        let mut inputs = Vec::new();
        for (_, candidate, _, _) in &members {
            let health = match bindings
                .get(&(
                    candidate.route_id,
                    candidate.account_id,
                    candidate.credential_generation,
                ))
                .map(|binding| binding.health.as_str())
            {
                Some("healthy") => GroupRoutingHealth::Healthy,
                Some("authentication") | None => GroupRoutingHealth::Authentication,
                Some("connection" | "unavailable" | "invalid_response") => {
                    GroupRoutingHealth::Transient
                }
                Some(_) => GroupRoutingHealth::HardQuota,
            };
            inputs.push(GroupRoutingCandidate {
                tenant_id: tenant_id.to_string(),
                route_id: candidate.route_id.to_string(),
                account_id: candidate.account_id.to_string(),
                generation: candidate.credential_generation as u64,
                health,
            });
        }
        let input = GroupRoutingInput {
            tenant_id: tenant_id.to_string(),
            seed,
            remaining_deadline_ms: remaining_deadline_ms
                .saturating_sub(started.elapsed().as_millis() as u64),
            config: config.clone(),
            candidates: inputs,
            quota_context,
        };
        let transient_signals = plugin_is_v2.then(|| {
            members
                .iter()
                .map(|member| GroupRoutingTransientSignal {
                    sample_count: member.2.transient_signal.sample_count.max(0) as u64,
                    ewma_micros: member.2.transient_signal.ewma_micros.clamp(0, 1_000_000) as u32,
                    last_observed_at: member.2.transient_signal.last_observed_at.max(0),
                    recovery_successes: member.2.transient_signal.recovery_successes.max(0) as u64,
                    revision: member.2.transient_signal.revision.max(0) as u64,
                })
                .collect::<Vec<_>>()
        });
        let runtime = state.plugins.clone();
        let execute_id = plugin_id.clone();
        let execute_input = input.clone();
        let result = tokio::time::timeout_at(
            hook_deadline,
            crate::api::plugin_execution::run_group(
                state.metrics.clone(),
                crate::metrics::plugin_execution::Phase::GroupRoutingPlan,
                move || {
                    runtime.execute_group_routing_plan_with_health(
                        &execute_id,
                        &execute_input,
                        transient_signals.as_deref(),
                    )
                },
            ),
        )
        .await
        .map_err(|_| AppError::Internal)
        .and_then(|result| result);
        match result {
            Ok(mut plan) => {
                // Sticky candidates form a deterministic, tenant/key/session
                // seeded rendezvous tier. Others preserve plugin plan order.
                sort_execution_candidates(selection_seed, &mut plan.candidates);
                for (position, execution) in plan.candidates.into_iter().enumerate() {
                    let directive = execution.directive;
                    let candidate = planned_candidate(&input, &directive)
                        .expect("validated exact candidate permutation")
                        .clone();
                    let key = (
                        Uuid::parse_str(&directive.route_id).map_err(|_| AppError::Internal)?,
                        Uuid::parse_str(&directive.account_id).map_err(|_| AppError::Internal)?,
                        directive.generation as i64,
                    );
                    ranks.insert(key, bucket_rank + position);
                    if !native_health {
                        let frozen_signal =
                            bindings.get(&key).filter(|_| plugin_is_v2).map(|binding| {
                                GroupRoutingTransientSignal {
                                    sample_count: binding.transient_signal.sample_count.max(0)
                                        as u64,
                                    ewma_micros: binding
                                        .transient_signal
                                        .ewma_micros
                                        .clamp(0, 1_000_000)
                                        as u32,
                                    last_observed_at: binding
                                        .transient_signal
                                        .last_observed_at
                                        .max(0),
                                    recovery_successes: binding
                                        .transient_signal
                                        .recovery_successes
                                        .max(0)
                                        as u64,
                                    revision: binding.transient_signal.revision.max(0) as u64,
                                }
                            });
                        policies.insert(
                            key,
                            CandidatePolicy {
                                plugin_id: plugin_id.clone(),
                                group_id: group_id.clone(),
                                strategy_version: members[0].3,
                                config: config.clone(),
                                candidate,
                                directive,
                                transient_policy: execution.transient_policy,
                                transient_signal_enabled: plugin_is_v2,
                                transient_signal: frozen_signal,
                            },
                        );
                    }
                }
                tracing::info!(%request_id, %group_id, %plugin_id, strategy_version=members[0].3, stage="group_routing_plan", "group strategy snapshot applied");
            }
            Err(error) => {
                // Fail closed with respect to authority, fail open only to the
                // native order/admission of these exact authorized candidates.
                tracing::warn!(%request_id, %group_id, %plugin_id, error_category=error.diagnostic_category(), stage="group_routing_native_fallback", "group strategy failed; native policy retained for its candidates");
            }
        }
    }
    candidates.sort_by_key(|candidate| {
        ranks
            .get(&(
                candidate.route_id,
                candidate.account_id,
                candidate.credential_generation,
            ))
            .copied()
            .unwrap_or(usize::MAX)
    });
    state.group_routing = Some(Arc::new(RequestGroupRouting {
        tenant_id,
        seed,
        started,
        deadline,
        policies,
    }));
    Ok(())
}

pub(crate) async fn observe_with_signal(
    state: &AppState,
    request_id: Uuid,
    route: Uuid,
    account: Uuid,
    generation: i64,
    outcome: GroupRoutingOutcome,
    transient_signal: Option<GroupRoutingTransientSignal>,
) -> Option<crate::plugin::routing::GroupRoutingExecutionDirective> {
    #[cfg(test)]
    test_observe_gate::wait(request_id).await;
    let snapshot = state.group_routing.as_ref()?;
    let policy = snapshot.policy(route, account, generation)?;
    let mut candidate = policy.candidate.clone();
    candidate.health = match outcome {
        GroupRoutingOutcome::Success => GroupRoutingHealth::Healthy,
        GroupRoutingOutcome::TransientFailure => GroupRoutingHealth::Transient,
        GroupRoutingOutcome::HardQuota => GroupRoutingHealth::HardQuota,
        GroupRoutingOutcome::Authentication => GroupRoutingHealth::Authentication,
        GroupRoutingOutcome::Cancelled => candidate.health,
    };
    let input = GroupRoutingObserveInput {
        tenant_id: snapshot.tenant_id.to_string(),
        seed: snapshot.seed,
        remaining_deadline_ms: snapshot
            .deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .as_millis() as u64,
        config: policy.config.clone(),
        candidate,
        outcome,
    };
    let runtime = state.plugins.clone();
    let plugin_id = policy.plugin_id.clone();
    let execution_signal = transient_signal.or_else(|| match outcome {
        GroupRoutingOutcome::HardQuota
        | GroupRoutingOutcome::Authentication
        | GroupRoutingOutcome::Cancelled => policy.transient_signal,
        GroupRoutingOutcome::Success | GroupRoutingOutcome::TransientFailure => None,
    });
    match crate::api::plugin_execution::run_group(
        state.metrics.clone(),
        crate::metrics::plugin_execution::Phase::GroupRoutingObserve,
        move || {
            runtime.execute_group_routing_observe_with_health(&plugin_id, &input, execution_signal)
        },
    )
    .await
    {
        Ok(directive) if directive.transient_policy == policy.transient_policy => Some(directive),
        Ok(_) => {
            tracing::warn!(%request_id, upstream_account_id=%account,stage="group_routing_observe_fallback",reason="policy_changed","group observe changed its frozen transient policy; native health policy retained");
            None
        }
        _ => {
            tracing::warn!(%request_id, upstream_account_id=%account,stage="group_routing_observe_fallback","group observe failed; native health policy retained");
            None
        }
    }
}

#[cfg(test)]
pub(crate) async fn observe(
    state: &AppState,
    request_id: Uuid,
    route: Uuid,
    account: Uuid,
    generation: i64,
    outcome: GroupRoutingOutcome,
) -> Option<GroupRoutingDirective> {
    observe_with_signal(state, request_id, route, account, generation, outcome, None)
        .await
        .map(|execution| execution.directive)
}

#[cfg(test)]
mod tests;
