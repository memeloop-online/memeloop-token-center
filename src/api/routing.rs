use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::IntoResponse,
};
use serde::Deserialize;
use uuid::Uuid;

use super::{management_tenant, require_service, require_service_tenant};
use crate::{
    AppState,
    db::{ReplaceCredentialRoutingInput, ReplaceRouteRoutingInput},
    error::AppError,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RouteRoutingQuery {
    tenant_external_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReplaceRouteRoutingRequest {
    tenant_external_id: String,
    #[serde(default)]
    upstream_account_ids: Vec<Uuid>,
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
    expected_updated_at: i64,
    expected_grant_revision: i64,
    #[serde(default)]
    custom_model_confirmed: bool,
}

pub(super) async fn get_route_routing(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(route_id): Path<Uuid>,
    Query(query): Query<RouteRoutingQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "routes:read").await?;
    let tenant = management_tenant(&service, Some(query.tenant_external_id))?
        .ok_or_else(|| AppError::BadRequest("tenant_external_id is required".into()))?;
    Ok(Json(state.db.route_routing(route_id, &tenant).await?))
}

pub(super) async fn replace_route_routing(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(route_id): Path<Uuid>,
    Json(body): Json<ReplaceRouteRoutingRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "routes:write").await?;
    require_service_tenant(&service, &body.tenant_external_id)?;
    Ok(Json(
        state
            .db
            .replace_route_routing(
                route_id,
                ReplaceRouteRoutingInput {
                    tenant_external_id: body.tenant_external_id,
                    upstream_account_ids: body.upstream_account_ids,
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
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CredentialRoutingQuery {
    tenant_external_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReplaceCredentialRoutingRequest {
    tenant_external_id: String,
    #[serde(default)]
    route_ids: Vec<Uuid>,
    #[serde(default)]
    route_group_ids: Vec<Uuid>,
    expected_grant_revision: i64,
}

pub(super) async fn get_credential_routing(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key_id): Path<Uuid>,
    Query(query): Query<CredentialRoutingQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "keys:read").await?;
    require_service_tenant(&service, &query.tenant_external_id)?;
    Ok(Json(
        state
            .db
            .credential_routing(key_id, &query.tenant_external_id)
            .await?,
    ))
}

pub(super) async fn replace_credential_routing(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key_id): Path<Uuid>,
    Json(body): Json<ReplaceCredentialRoutingRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "keys:write").await?;
    require_service_tenant(&service, &body.tenant_external_id)?;
    Ok(Json(
        state
            .db
            .replace_credential_routing(
                key_id,
                ReplaceCredentialRoutingInput {
                    tenant_external_id: body.tenant_external_id,
                    route_ids: body.route_ids,
                    route_group_ids: body.route_group_ids,
                    expected_grant_revision: body.expected_grant_revision,
                },
            )
            .await?,
    ))
}
