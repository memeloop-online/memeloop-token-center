//! Media adapter for the shared group-routing-v1 runtime and core health gate.
use crate::{
    AppState,
    api::{MediaAttemptBudget, MediaAttemptGuard},
    db::{RouteSelectionOptions, UpstreamAttemptAdmission},
    error::AppError,
    model::{AuthenticatedKey, GenerationJobWork},
    provider::ResolvedUpstream,
};
use uuid::Uuid;

pub(crate) fn snapshot(state: &AppState) -> Result<Option<serde_json::Value>, AppError> {
    crate::group_routing::durable::capture(state)
}

pub(crate) async fn restore(
    state: &AppState,
    job: &GenerationJobWork,
) -> Result<AppState, AppError> {
    crate::group_routing::durable::restore(state, job).await
}

pub(crate) async fn prepare_route(
    state: &mut AppState,
    key: &AuthenticatedKey,
    model: &str,
    hint: Option<Uuid>,
    seed: Uuid,
    request_id: Uuid,
) -> Result<ResolvedUpstream, AppError> {
    prepare_route_for_protocol(state, key, model, "generation", hint, seed, request_id).await
}

pub(crate) async fn prepare_route_for_protocol(
    state: &mut AppState,
    key: &AuthenticatedKey,
    model: &str,
    protocol: &str,
    hint: Option<Uuid>,
    seed: Uuid,
    request_id: Uuid,
) -> Result<ResolvedUpstream, AppError> {
    let mut candidates = state
        .db
        .list_authorized_upstream_candidates_with_hint(
            key.key_id,
            key.tenant_id,
            model,
            protocol,
            RouteSelectionOptions {
                upstream_account_hint: hint,
                avoid_route_account: None,
                selection_seed: seed,
            },
        )
        .await?;
    let mut native = None;
    for candidate in &candidates {
        if let Some(route) = state
            .db
            .materialize_authorized_upstream_candidate(
                candidate,
                state.config.key_pepper.as_bytes(),
            )
            .await?
        {
            native = Some(route);
            break;
        }
    }
    let native =
        native.ok_or_else(|| AppError::Upstream(format!("{protocol} route is not configured")))?;
    if !state.plugins.has_group_routing_hooks() {
        return Ok(native);
    }
    let mut attempt_budget = MediaAttemptBudget::from_primary(&native, request_id)?;
    attempt_budget.arm();
    let deadline = attempt_budget.recovery_wait_deadline(state.config.upstream_health);
    crate::group_routing::prepare(
        state,
        key.tenant_id,
        seed,
        request_id,
        deadline,
        &mut candidates,
    )
    .await?;
    if state.group_routing.is_none() {
        return Ok(native);
    }
    // Only the original authorized tuples can be materialized. Membership or
    // credential changes after planning are revalidated by the core resolver.
    for candidate in &candidates {
        if let Some(route) = state
            .db
            .materialize_authorized_upstream_candidate(
                candidate,
                state.config.key_pepper.as_bytes(),
            )
            .await?
        {
            return Ok(route);
        }
    }
    Err(AppError::Upstream(format!(
        "{protocol} candidates are unavailable"
    )))
}

pub(crate) async fn admit(
    state: &AppState,
    tenant_id: Uuid,
    request_id: Uuid,
    route: &ResolvedUpstream,
) -> Result<MediaAttemptGuard, AppError> {
    if let Some(snapshot) = state.group_routing.as_ref()
        && snapshot.tenant_id != tenant_id
    {
        return Err(AppError::Forbidden);
    }
    // A customer generation uses the selected account. Its success refreshes
    // service availability through the same attempt terminal as a proxy send.
    Ok(MediaAttemptGuard::new(
        state,
        request_id,
        route.route_id,
        route.account_id,
        route.credential_generation,
        route.transport_revision,
        UpstreamAttemptAdmission::Observed,
        None,
    ))
}
