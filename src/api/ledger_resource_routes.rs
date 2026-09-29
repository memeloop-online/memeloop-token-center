use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use super::require_service;
use crate::{AppState, error::AppError};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PlanLedgerResourceRouteSwitchRequest {
    tenant_external_id: String,
    route_id: Uuid,
    source_upstream_account_id: Uuid,
    target_upstream_account_id: Uuid,
    expected_route_updated_at: i64,
    expected_source_updated_at: i64,
    expected_target_updated_at: i64,
    expected_grant_revision: i64,
}

pub(super) async fn plan_ledger_resource_route_switch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PlanLedgerResourceRouteSwitchRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_switch_scopes(&headers, &state).await?;
    require_explicit_tenant(&service, &body.tenant_external_id)?;
    Ok(Json(
        state
            .db
            .plan_ledger_resource_route_switch(
                &body.tenant_external_id,
                body.route_id,
                body.source_upstream_account_id,
                body.target_upstream_account_id,
                body.expected_route_updated_at,
                body.expected_source_updated_at,
                body.expected_target_updated_at,
                body.expected_grant_revision,
                service.service_id,
            )
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LedgerResourceRouteSwitchTenant {
    tenant_external_id: String,
}

pub(super) async fn apply_ledger_resource_route_switch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(operation_id): Path<Uuid>,
    Json(body): Json<LedgerResourceRouteSwitchTenant>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_switch_scopes(&headers, &state).await?;
    require_explicit_tenant(&service, &body.tenant_external_id)?;
    Ok(Json(
        state
            .db
            .apply_ledger_resource_route_switch(&body.tenant_external_id, operation_id)
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RollbackLedgerResourceRouteSwitchRequest {
    tenant_external_id: String,
    expected_route_updated_at: i64,
    expected_grant_revision: i64,
}

pub(super) async fn rollback_ledger_resource_route_switch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(operation_id): Path<Uuid>,
    Json(body): Json<RollbackLedgerResourceRouteSwitchRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_switch_scopes(&headers, &state).await?;
    require_explicit_tenant(&service, &body.tenant_external_id)?;
    let result: Value = state
        .db
        .rollback_ledger_resource_route_switch(
            &body.tenant_external_id,
            operation_id,
            body.expected_route_updated_at,
            body.expected_grant_revision,
        )
        .await?;
    Ok(Json(result))
}

async fn require_switch_scopes(
    headers: &HeaderMap,
    state: &AppState,
) -> Result<crate::model::AuthenticatedService, AppError> {
    let service = require_service(headers, state, "credits:write").await?;
    if !service.allows("routes:write") {
        return Err(AppError::Forbidden);
    }
    Ok(service)
}

fn require_explicit_tenant(
    service: &crate::model::AuthenticatedService,
    tenant_external_id: &str,
) -> Result<(), AppError> {
    if tenant_external_id.trim().is_empty() || tenant_external_id.trim() == "*" {
        return Err(AppError::BadRequest(
            "tenant_external_id must name one explicit tenant".into(),
        ));
    }
    super::require_service_tenant(service, tenant_external_id)
}
