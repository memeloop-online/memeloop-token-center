//! Request-owned group policy. Authorization and health leases stay in core.
use crate::{
    AppState,
    error::AppError,
    plugin::routing::{
        GroupRoutingCandidate, GroupRoutingDirective, GroupRoutingHealth, GroupRoutingInput,
        GroupRoutingObserveInput, GroupRoutingOutcome,
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

#[derive(Clone)]
pub(crate) struct CandidatePolicy {
    plugin_id: String,
    config: serde_json::Value,
    candidate: GroupRoutingCandidate,
    directive: GroupRoutingDirective,
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
}

impl CandidatePolicy {
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
        .map(|(route, account, binding)| ((route, account), binding))
        .collect::<BTreeMap<_, _>>();
    if bindings.is_empty() {
        return Ok(());
    }
    for (index, candidate) in candidates.iter().enumerate() {
        if let Some(binding) = bindings.get(&(candidate.route_id, candidate.account_id)) {
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
    let mut policies = BTreeMap::new();
    let mut ranks = BTreeMap::new();
    let mut next_rank = 0usize;
    for ((_, group_id), members) in buckets {
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
        let mut inputs = Vec::new();
        for (_, candidate, _, _) in &members {
            let health = match bindings
                .get(&(candidate.route_id, candidate.account_id))
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
        };
        let runtime = state.plugins.clone();
        let execute_id = plugin_id.clone();
        let execute_input = input.clone();
        let result = tokio::time::timeout_at(
            hook_deadline,
            tokio::task::spawn_blocking(move || {
                runtime.execute_group_routing_plan(&execute_id, &execute_input)
            }),
        )
        .await
        .map_err(|_| AppError::Internal)
        .and_then(|result| result.map_err(|_| AppError::Internal))
        .and_then(|result| result);
        match result {
            Ok(mut plan) => {
                // Sticky candidates form a deterministic, tenant/key/session
                // seeded rendezvous tier. Others preserve plugin plan order.
                sort_plan_candidates(selection_seed, &mut plan.candidates);
                for directive in plan.candidates {
                    let candidate = input
                        .candidates
                        .iter()
                        .find(|candidate| {
                            candidate.route_id == directive.route_id
                                && candidate.account_id == directive.account_id
                        })
                        .expect("validated exact candidate permutation")
                        .clone();
                    let key = (
                        Uuid::parse_str(&directive.route_id).map_err(|_| AppError::Internal)?,
                        Uuid::parse_str(&directive.account_id).map_err(|_| AppError::Internal)?,
                        directive.generation as i64,
                    );
                    ranks.insert(key, next_rank);
                    next_rank += 1;
                    policies.insert(
                        key,
                        CandidatePolicy {
                            plugin_id: plugin_id.clone(),
                            config: config.clone(),
                            candidate,
                            directive,
                        },
                    );
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

pub(crate) async fn observe(
    state: &AppState,
    request_id: Uuid,
    route: Uuid,
    account: Uuid,
    generation: i64,
    outcome: GroupRoutingOutcome,
) -> Option<GroupRoutingDirective> {
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
    match tokio::task::spawn_blocking(move || {
        runtime.execute_group_routing_observe(&plugin_id, &input)
    })
    .await
    {
        Ok(Ok(directive)) => Some(directive),
        _ => {
            tracing::warn!(%request_id, upstream_account_id=%account,stage="group_routing_observe_fallback","group observe failed; native health policy retained");
            None
        }
    }
}

#[cfg(test)]
mod tests;
