use super::{require_service, require_service_tenant};
use crate::{AppState, error::AppError, model::AuthenticatedService};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GrantQuery {
    tenant_external_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PutGrant {
    enabled: bool,
    expected_revision: i64,
}

fn tenant(query: &GrantQuery) -> Result<&str, AppError> {
    let tenant = query.tenant_external_id.trim();
    if tenant.is_empty() || tenant.len() > 200 {
        return Err(AppError::BadRequest(
            "background action requires an explicit tenant".into(),
        ));
    }
    Ok(tenant)
}

async fn response(
    state: &AppState,
    plugin: &str,
    account: Uuid,
    tenant: &str,
    service: &AuthenticatedService,
) -> Result<Response, AppError> {
    let grant = state
        .db
        .background_action_grant(plugin, tenant, &account.to_string())
        .await?;
    let operation = match grant
        .as_ref()
        .and_then(|grant| grant.last_operation_id.as_deref())
    {
        Some(id) => Some(
            state
                .db
                .quota_reset_operation_for_tenant(tenant, &account.to_string(), id)
                .await?,
        ),
        None => {
            state
                .db
                .current_quota_reset_operation_for_tenant(tenant, &account.to_string())
                .await?
        }
    };
    Ok((
        [(header::CACHE_CONTROL, "private, no-store")],
        Json(serde_json::json!({
            "grant":grant,"can_write":service.allows("providers:write")&&service.allows("plugins:write"),"operation":operation,
        })),
    ).into_response())
}

pub(super) async fn get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((plugin, account)): Path<(String, Uuid)>,
    Query(query): Query<GrantQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "plugins:read").await?;
    require_service(&headers, &state, "providers:read").await?;
    let tenant = tenant(&query)?;
    require_service_tenant(&service, tenant)?;
    state.db.require_upstream_tenant(account, tenant).await?;
    response(&state, &plugin, account, tenant, &service).await
}

pub(super) async fn put(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((plugin, account)): Path<(String, Uuid)>,
    Query(query): Query<GrantQuery>,
    Json(body): Json<PutGrant>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "plugins:write").await?;
    require_service(&headers, &state, "providers:write").await?;
    let tenant = tenant(&query)?;
    require_service_tenant(&service, tenant)?;
    state.db.require_upstream_tenant(account, tenant).await?;
    let state = state.pin_application_plugins().await?;
    if body.enabled && !state.plugins.has_background_actions(&plugin) {
        return Err(AppError::BadRequest(
            "plugin has no installed background action contribution".into(),
        ));
    }
    let hash = if service.service_id.is_none() {
        crate::plugin::background_actions::bootstrap_authority_hash(&state)
    } else {
        String::new()
    };
    state
        .db
        .put_background_action_grant(
            &plugin,
            tenant,
            account,
            body.enabled,
            body.expected_revision,
            &service,
            &hash,
        )
        .await?;
    response(&state, &plugin, account, tenant, &service).await
}
