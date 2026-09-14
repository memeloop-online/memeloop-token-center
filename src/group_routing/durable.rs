//! Versioned, credential-free persistence of an already-authorized media plan.
//! Recovery never runs plan again or substitutes current group configuration.
use super::*;
use serde::{Deserialize, Serialize};

const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurablePlan {
    version: u8,
    tenant_id: Uuid,
    seed: u64,
    started_at: i64,
    deadline_at: i64,
    application_revision: Option<i64>,
    fingerprints: BTreeMap<String, String>,
    policies: Vec<CandidatePolicy>,
}

pub(crate) fn capture(state: &AppState) -> Result<Option<serde_json::Value>, AppError> {
    let Some(snapshot) = state.group_routing.as_ref() else {
        return Ok(None);
    };
    let now = crate::db::unix_millis();
    let started_at = now.saturating_sub(snapshot.started.elapsed().as_millis() as i64);
    let deadline_at = now.saturating_add(
        snapshot
            .deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .as_millis() as i64,
    );
    #[cfg(feature = "experimental-plugin-revisions")]
    let application_revision = state
        .pinned_application_plugins
        .as_ref()
        .map(|snapshot| snapshot.receipt.revision);
    #[cfg(not(feature = "experimental-plugin-revisions"))]
    let application_revision = None;
    let mut fingerprints = state.plugins.group_routing_fingerprints();
    fingerprints.retain(|id, _| {
        snapshot
            .policies
            .values()
            .any(|policy| &policy.plugin_id == id)
    });
    let value = serde_json::to_value(DurablePlan {
        version: 1,
        tenant_id: snapshot.tenant_id,
        seed: snapshot.seed,
        started_at,
        deadline_at,
        application_revision,
        fingerprints,
        policies: snapshot.policies.values().cloned().collect(),
    })
    .map_err(|_| AppError::Internal)?;
    if serde_json::to_vec(&value)
        .map_err(|_| AppError::Internal)?
        .len()
        > MAX_SNAPSHOT_BYTES
    {
        return Err(AppError::BadRequest(
            "media routing snapshot exceeds its bound".into(),
        ));
    }
    Ok(Some(value))
}

pub(crate) async fn restore(
    state: &AppState,
    job: &crate::model::GenerationJobWork,
) -> Result<AppState, AppError> {
    restore_selected(
        state,
        job.routing_snapshot.as_ref(),
        job.tenant_id,
        job.model_route_id,
        job.upstream_account_id,
        job.job_id,
    )
    .await
}

pub(crate) async fn restore_selected(
    state: &AppState,
    value: Option<&serde_json::Value>,
    tenant_id: Uuid,
    model_route_id: Option<Uuid>,
    upstream_account_id: Uuid,
    job_id: Uuid,
) -> Result<AppState, AppError> {
    let mut restored = state.clone();
    restored.group_routing = None;
    let Some(value) = value else {
        return Ok(restored);
    };
    if serde_json::to_vec(value)
        .map_err(|_| AppError::Internal)?
        .len()
        > MAX_SNAPSHOT_BYTES
    {
        return Err(AppError::Internal);
    }
    let stored: DurablePlan =
        serde_json::from_value(value.clone()).map_err(|_| AppError::Internal)?;
    if stored.version != 1
        || stored.tenant_id != tenant_id
        || stored.policies.len() > 1024
        || stored.deadline_at < stored.started_at
    {
        return Err(AppError::Internal);
    }
    let total_budget = stored.deadline_at.saturating_sub(stored.started_at) as u64;
    // A stored deadline is a bound, never a new budget on each worker claim.
    if total_budget > 24 * 60 * 60 * 1000 {
        return Err(AppError::Internal);
    }
    let mut runtime_available = true;
    #[cfg(feature = "experimental-plugin-revisions")]
    if let Some(revision) = stored.application_revision {
        if let Some(authority) = restored.application_plugins.as_ref() {
            match authority.pin_historical(revision).await {
                Ok(snapshot) => {
                    restored = restored.with_pinned_application_plugins(snapshot);
                }
                Err(_) => runtime_available = false,
            }
        } else {
            runtime_available = false;
        }
    }
    #[cfg(not(feature = "experimental-plugin-revisions"))]
    if stored.application_revision.is_some() {
        runtime_available = false;
    }
    let available = restored.plugins.group_routing_fingerprints();
    let mut policies = BTreeMap::new();
    for policy in stored.policies {
        let input = GroupRoutingInput {
            tenant_id: tenant_id.to_string(),
            seed: stored.seed,
            remaining_deadline_ms: total_budget,
            config: policy.config.clone(),
            candidates: vec![policy.candidate.clone()],
        };
        crate::plugin::routing::validate_group_routing_plan(
            &input,
            &crate::plugin::routing::GroupRoutingPlan {
                candidates: vec![policy.directive.clone()],
            },
        )?;
        let route = Uuid::parse_str(&policy.candidate.route_id).map_err(|_| AppError::Internal)?;
        let account =
            Uuid::parse_str(&policy.candidate.account_id).map_err(|_| AppError::Internal)?;
        if model_route_id != Some(route) || account != upstream_account_id {
            continue;
        }
        let fingerprint = stored.fingerprints.get(&policy.plugin_id);
        if !runtime_available
            || fingerprint.is_none()
            || available.get(&policy.plugin_id) != fingerprint
        {
            tracing::warn!(%job_id, stage = "media_group_routing_revision_unavailable", "pinned strategy unavailable; native health policy retained without running replacement code");
            continue;
        }
        let generation =
            i64::try_from(policy.candidate.generation).map_err(|_| AppError::Internal)?;
        if policies
            .insert((route, account, generation), policy)
            .is_some()
        {
            return Err(AppError::Internal);
        }
    }
    let now = crate::db::unix_millis();
    let instant = tokio::time::Instant::now();
    let elapsed = Duration::from_millis(now.saturating_sub(stored.started_at).max(0) as u64);
    restored.group_routing = Some(Arc::new(RequestGroupRouting {
        tenant_id,
        seed: stored.seed,
        started: instant.checked_sub(elapsed).unwrap_or(instant),
        deadline: instant
            + Duration::from_millis(stored.deadline_at.saturating_sub(now).max(0) as u64),
        policies,
    }));
    Ok(restored)
}

pub(crate) fn deadline(state: &AppState) -> Option<tokio::time::Instant> {
    state
        .group_routing
        .as_ref()
        .map(|snapshot| snapshot.deadline)
}
