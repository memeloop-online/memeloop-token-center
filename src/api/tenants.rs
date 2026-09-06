use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use serde::Deserialize;

use super::{AppError, AppState, require_global_service, require_service};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct TenantIdentifierRequest {
    external_id: String,
}

pub(in crate::api) async fn list_tenant_management(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl axum::response::IntoResponse, AppError> {
    let service = require_service(&headers, &state, "tenants:read").await?;
    require_global_service(&service)?;
    Ok(Json(state.db.list_tenant_management().await?))
}

pub(in crate::api) async fn create_tenant(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<TenantIdentifierRequest>,
) -> Result<impl axum::response::IntoResponse, AppError> {
    let service = require_service(&headers, &state, "tenants:write").await?;
    require_global_service(&service)?;
    Ok((
        StatusCode::CREATED,
        Json(
            state
                .db
                .create_tenant(&body.external_id, service.service_id)
                .await?,
        ),
    ))
}

pub(in crate::api) async fn rename_tenant(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(current_external_id): Path<String>,
    Json(body): Json<TenantIdentifierRequest>,
) -> Result<impl axum::response::IntoResponse, AppError> {
    let service = require_service(&headers, &state, "tenants:write").await?;
    require_global_service(&service)?;
    Ok(Json(
        state
            .db
            .rename_tenant(&current_external_id, &body.external_id, service.service_id)
            .await?,
    ))
}

pub(in crate::api) async fn archive_tenant(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(external_id): Path<String>,
) -> Result<impl axum::response::IntoResponse, AppError> {
    let service = require_service(&headers, &state, "tenants:write").await?;
    require_global_service(&service)?;
    Ok(Json(
        state
            .db
            .set_tenant_archived(&external_id, true, service.service_id)
            .await?,
    ))
}

pub(in crate::api) async fn restore_tenant(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(external_id): Path<String>,
) -> Result<impl axum::response::IntoResponse, AppError> {
    let service = require_service(&headers, &state, "tenants:write").await?;
    require_global_service(&service)?;
    Ok(Json(
        state
            .db
            .set_tenant_archived(&external_id, false, service.service_id)
            .await?,
    ))
}

pub(in crate::api) async fn delete_tenant(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(external_id): Path<String>,
) -> Result<impl axum::response::IntoResponse, AppError> {
    let service = require_service(&headers, &state, "tenants:write").await?;
    require_global_service(&service)?;
    state
        .db
        .delete_archived_tenant(&external_id, service.service_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
