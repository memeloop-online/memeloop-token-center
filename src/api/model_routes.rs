use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde::Deserialize;
use uuid::Uuid;

use super::{
    ManagementTenantQuery, default_tenant, management_tenant, require_service,
    require_service_tenant,
};
use crate::{
    AppState,
    db::{CreateModelRouteInput, UpdateModelRouteInput},
    error::AppError,
};

#[derive(Debug, Deserialize)]
pub(super) struct CreateModelRouteRequest {
    #[serde(default = "default_tenant")]
    tenant_external_id: String,
    public_model: String,
    upstream_account_id: Uuid,
    upstream_model: String,
    protocol: String,
    #[serde(default)]
    priority: i64,
}

pub(super) async fn create_model_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateModelRouteRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "routes:write").await?;
    require_service_tenant(&service, &body.tenant_external_id)?;
    state
        .db
        .require_upstream_tenant(body.upstream_account_id, &body.tenant_external_id)
        .await?;
    let driver = state.db.upstream_driver(body.upstream_account_id).await?;
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
    let route = state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: body.tenant_external_id,
            public_model: body.public_model,
            upstream_account_id: body.upstream_account_id,
            upstream_model: body.upstream_model,
            protocol: body.protocol,
            priority: body.priority,
        })
        .await?;
    Ok((StatusCode::CREATED, Json(route)))
}

pub(super) async fn list_model_routes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ManagementTenantQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "routes:read").await?;
    let tenant = management_tenant(&service, query.tenant_external_id)?;
    Ok(Json(state.db.list_model_routes(tenant.as_deref()).await?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UpdateModelRouteRequest {
    tenant_external_id: String,
    public_model: String,
    upstream_account_id: Uuid,
    upstream_model: String,
    protocol: String,
    priority: i64,
    expected_updated_at: i64,
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
    state
        .db
        .require_upstream_tenant(body.upstream_account_id, &tenant)
        .await?;
    let driver = state.db.upstream_driver(body.upstream_account_id).await?;
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
    Ok(Json(
        state
            .db
            .update_model_route(
                route_id,
                &tenant,
                UpdateModelRouteInput {
                    public_model: body.public_model,
                    upstream_account_id: body.upstream_account_id,
                    upstream_model: body.upstream_model,
                    protocol: body.protocol,
                    priority: body.priority,
                    expected_updated_at: body.expected_updated_at,
                },
            )
            .await?,
    ))
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
