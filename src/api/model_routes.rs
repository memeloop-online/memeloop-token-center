use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use super::{default_tenant, management_tenant, require_service, require_service_tenant};
use crate::{
    AppState,
    db::{CreateRoutedModelRouteInput, UpdateRoutedModelRouteInput},
    error::AppError,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateModelRouteRequest {
    #[serde(default = "default_tenant")]
    tenant_external_id: String,
    public_model: String,
    #[serde(default)]
    upstream_account_id: Option<Uuid>,
    #[serde(default)]
    upstream_account_ids: Vec<Uuid>,
    upstream_model: String,
    protocol: String,
    #[serde(default)]
    priority: i64,
    #[serde(default)]
    included_provider_group_ids: Vec<Uuid>,
    #[serde(default)]
    excluded_provider_group_ids: Vec<Uuid>,
    #[serde(default)]
    route_group_ids: Vec<Uuid>,
    #[serde(default)]
    route_group_names: Vec<String>,
    #[serde(default)]
    granted_credential_ids: Vec<Uuid>,
    #[serde(default)]
    custom_model_confirmed: bool,
}

pub(super) async fn create_model_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateModelRouteRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "routes:write").await?;
    require_service_tenant(&service, &body.tenant_external_id)?;
    let mut upstream_account_ids = body.upstream_account_ids;
    if let Some(legacy_account_id) = body.upstream_account_id {
        upstream_account_ids.push(legacy_account_id);
    }
    upstream_account_ids.sort_unstable();
    upstream_account_ids.dedup();
    for account_id in &upstream_account_ids {
        state
            .db
            .require_upstream_tenant(*account_id, &body.tenant_external_id)
            .await?;
        let driver = state.db.upstream_driver(*account_id).await?;
        let provider = state
            .providers
            .get(&driver)
            .ok_or_else(|| AppError::BadRequest(format!("unknown provider driver: {driver}")))?;
        if !provider
            .protocols
            .iter()
            .any(|value| value == &body.protocol)
        {
            return Err(AppError::BadRequest(format!(
                "provider {driver} does not support the {} protocol",
                body.protocol
            )));
        }
    }
    let (route, routing) = state
        .db
        .create_routed_model_route(CreateRoutedModelRouteInput {
            tenant_external_id: body.tenant_external_id,
            public_model: body.public_model,
            upstream_model: body.upstream_model,
            protocol: body.protocol,
            priority: body.priority,
            upstream_account_ids,
            included_provider_group_ids: body.included_provider_group_ids,
            excluded_provider_group_ids: body.excluded_provider_group_ids,
            route_group_ids: body.route_group_ids,
            route_group_names: body.route_group_names,
            granted_credential_ids: body.granted_credential_ids,
            custom_model_confirmed: body.custom_model_confirmed,
        })
        .await?;
    let mut value = serde_json::to_value(route).map_err(|_| AppError::Internal)?;
    let Value::Object(ref mut object) = value else {
        return Err(AppError::Internal);
    };
    let routing = serde_json::to_value(routing).map_err(|_| AppError::Internal)?;
    let Value::Object(routing) = routing else {
        return Err(AppError::Internal);
    };
    object.extend(routing);
    Ok((StatusCode::CREATED, Json(value)))
}

pub(super) async fn list_model_routes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ModelRouteListQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "routes:read").await?;
    let tenant = management_tenant(&service, query.tenant_external_id)?;
    Ok(Json(
        state
            .db
            .list_enriched_model_routes_page(
                tenant.as_deref(),
                query.before_created_at,
                query.before_id,
                query.limit,
            )
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
pub(super) struct ModelRouteListQuery {
    tenant_external_id: Option<String>,
    before_created_at: Option<i64>,
    before_id: Option<Uuid>,
    #[serde(default = "default_model_route_list_limit")]
    limit: i64,
}

fn default_model_route_list_limit() -> i64 {
    100
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UpdateModelRouteRequest {
    tenant_external_id: String,
    public_model: String,
    #[serde(default)]
    upstream_account_id: Option<Uuid>,
    #[serde(default)]
    upstream_account_ids: Vec<Uuid>,
    upstream_model: String,
    protocol: String,
    priority: i64,
    #[serde(default)]
    included_provider_group_ids: Vec<Uuid>,
    #[serde(default)]
    excluded_provider_group_ids: Vec<Uuid>,
    #[serde(default)]
    route_group_ids: Vec<Uuid>,
    #[serde(default)]
    route_group_names: Vec<String>,
    #[serde(default)]
    granted_credential_ids: Vec<Uuid>,
    #[serde(default)]
    custom_model_confirmed: bool,
    expected_updated_at: i64,
    expected_grant_revision: i64,
}

pub(super) async fn update_model_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(route_id): Path<Uuid>,
    Json(body): Json<UpdateModelRouteRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "routes:write").await?;
    let tenant = management_tenant(&service, Some(body.tenant_external_id))?
        .ok_or_else(|| AppError::BadRequest("tenant_external_id is required".into()))?;
    let mut upstream_account_ids = body.upstream_account_ids;
    if let Some(legacy_account_id) = body.upstream_account_id {
        upstream_account_ids.push(legacy_account_id);
    }
    upstream_account_ids.sort_unstable();
    upstream_account_ids.dedup();
    for account_id in &upstream_account_ids {
        state
            .db
            .require_upstream_tenant(*account_id, &tenant)
            .await?;
        let driver = state.db.upstream_driver(*account_id).await?;
        let provider = state
            .providers
            .get(&driver)
            .ok_or_else(|| AppError::BadRequest(format!("unknown provider driver: {driver}")))?;
        if !provider
            .protocols
            .iter()
            .any(|value| value == &body.protocol)
        {
            return Err(AppError::BadRequest(format!(
                "provider {driver} does not support the {} protocol",
                body.protocol
            )));
        }
    }
    let (route, routing) = state
        .db
        .update_routed_model_route(
            route_id,
            UpdateRoutedModelRouteInput {
                tenant_external_id: tenant,
                public_model: body.public_model,
                upstream_model: body.upstream_model,
                protocol: body.protocol,
                priority: body.priority,
                upstream_account_ids,
                included_provider_group_ids: body.included_provider_group_ids,
                excluded_provider_group_ids: body.excluded_provider_group_ids,
                route_group_ids: body.route_group_ids,
                route_group_names: body.route_group_names,
                granted_credential_ids: body.granted_credential_ids,
                expected_updated_at: body.expected_updated_at,
                expected_grant_revision: body.expected_grant_revision,
                custom_model_confirmed: body.custom_model_confirmed,
            },
        )
        .await?;
    let mut value = serde_json::to_value(route).map_err(|_| AppError::Internal)?;
    let Value::Object(ref mut object) = value else {
        return Err(AppError::Internal);
    };
    let Value::Object(routing) = serde_json::to_value(routing).map_err(|_| AppError::Internal)?
    else {
        return Err(AppError::Internal);
    };
    object.extend(routing);
    Ok(Json(value))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SetModelRouteEnabledRequest {
    tenant_external_id: String,
    enabled: bool,
    expected_updated_at: i64,
}

pub(super) async fn set_model_route_enabled(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(route_id): Path<Uuid>,
    Json(body): Json<SetModelRouteEnabledRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "routes:write").await?;
    let tenant = management_tenant(&service, Some(body.tenant_external_id))?
        .ok_or_else(|| AppError::BadRequest("tenant_external_id is required".into()))?;
    Ok(Json(
        state
            .db
            .set_model_route_enabled(route_id, &tenant, body.enabled, body.expected_updated_at)
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeleteModelRouteQuery {
    tenant_external_id: String,
    expected_updated_at: i64,
}

pub(super) async fn delete_model_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(route_id): Path<Uuid>,
    Query(query): Query<DeleteModelRouteQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "routes:write").await?;
    let tenant = management_tenant(&service, Some(query.tenant_external_id))?
        .ok_or_else(|| AppError::BadRequest("tenant_external_id is required".into()))?;
    state
        .db
        .delete_model_route(route_id, &tenant, query.expected_updated_at)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
