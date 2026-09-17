//! Media adapter for the shared group-routing-v1 runtime and core health gate.
use crate::{
    AppState,
    api::{MediaAttemptBudget, MediaAttemptGuard, wait_media_recovery},
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
                avoid_upstream_account_id: None,
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
    let deadline = MediaAttemptBudget::from_primary(&native, request_id)?
        .recovery_wait_deadline(state.config.upstream_health);
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
    let policy = state.group_routing.as_ref().and_then(|snapshot| {
        (snapshot.tenant_id == tenant_id)
            .then(|| {
                snapshot.policy(
                    route.route_id,
                    route.account_id,
                    route.credential_generation,
                )
            })
            .flatten()
    });
    if let Some(snapshot) = state.group_routing.as_ref()
        && snapshot.tenant_id != tenant_id
    {
        return Err(AppError::Forbidden);
    }
    let admission = if let Some(policy) = policy {
        state
            .db
            .claim_upstream_account_attempt_with_strategy(
                tenant_id,
                route.account_id,
                route.credential_generation,
                state.config.upstream_health,
                policy.allow_probe(),
                Some(policy.cooldown_ms()),
                false,
            )
            .await?
    } else {
        state
            .db
            .claim_upstream_account_attempt_with_health_config(
                route.account_id,
                route.credential_generation,
                state.config.upstream_health,
            )
            .await?
    };
    match admission {
        UpstreamAttemptAdmission::Unavailable {
            transient_wait_eligible: true,
            ..
        } => {
            let deadline = crate::group_routing::durable::deadline(state)
                .unwrap_or(tokio::time::Instant::now());
            if let Some((refreshed, _, mut guard)) =
                wait_media_recovery(state, request_id, route.clone(), deadline).await?
            {
                // The caller already prepared a request with this exact route.
                // Never send its old credential after a recovery refresh rotates it.
                if refreshed.credential_generation == route.credential_generation
                    && refreshed.transport_revision == route.transport_revision
                {
                    return Ok(guard);
                }
                guard.abandon_without_observe().await;
            }
            Err(AppError::Upstream(
                "generation upstream is cooling down".into(),
            ))
        }
        UpstreamAttemptAdmission::Unavailable { .. } => {
            Err(AppError::Upstream("generation upstream is isolated".into()))
        }
        admission => Ok(MediaAttemptGuard::new(
            state,
            request_id,
            route.route_id,
            route.account_id,
            route.credential_generation,
            admission,
            None,
        )),
    }
}
