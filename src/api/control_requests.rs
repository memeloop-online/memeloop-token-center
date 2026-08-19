use std::{convert::Infallible, time::Duration};

use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::sse::{Event, KeepAlive, Sse},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use super::{
    RequestsQuery, StatsQuery, generation_asset_response, management_tenant,
    request_detail_response, require_service,
};
use crate::{AppState, db::unix_millis, error::AppError};

pub(super) async fn provider_types(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    require_service(&headers, &state, "providers:read").await?;
    Ok(Json(state.providers.list().to_vec()))
}

pub(super) async fn plugin_manifests(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    require_service(&headers, &state, "plugins:read").await?;
    Ok(Json(state.plugins.manifests()))
}

pub(super) async fn configuration_schemas(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    require_service(&headers, &state, "schemas:read").await?;
    fn schema(source: &str) -> Result<Value, AppError> {
        serde_json::from_str(source).map_err(|_| AppError::Internal)
    }
    Ok(Json(json!({
        "core_config": schema(include_str!("../../schemas/core-config.schema.json"))?,
        "key_create": schema(include_str!("../../schemas/key-create.schema.json"))?,
        "key_policy": schema(include_str!("../../schemas/key-policy.schema.json"))?,
        "generation_create": schema(include_str!("../../schemas/generation-create.schema.json"))?,
        "generation_price": schema(include_str!("../../schemas/generation-price.schema.json"))?,
        "model_price": schema(include_str!("../../schemas/model-price.schema.json"))?,
        "model_route": schema(include_str!("../../schemas/model-route.schema.json"))?,
        "plugin_manifest": schema(include_str!("../../schemas/plugin-manifest.schema.json"))?,
        "provider_account": schema(include_str!("../../schemas/provider-account.schema.json"))?,
        "service_token": schema(include_str!("../../schemas/service-token.schema.json"))?
    })))
}

pub(super) async fn list_tenants(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TenantListQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "requests:read").await?;
    if let Some(scoped_tenant) = service.tenant_external_id {
        let visible = query
            .after_external_id
            .as_deref()
            .is_none_or(|after| scoped_tenant.as_str() > after);
        return Ok(Json(if visible {
            vec![crate::model::TenantView {
                external_id: scoped_tenant,
            }]
        } else {
            Vec::new()
        }));
    }
    Ok(Json(
        state
            .db
            .list_tenants_page(query.after_external_id.as_deref(), query.limit)
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
pub(super) struct TenantListQuery {
    after_external_id: Option<String>,
    #[serde(default = "default_control_list_limit")]
    limit: i64,
}

fn default_control_list_limit() -> i64 {
    100
}

pub(super) async fn internal_requests(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RequestsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "requests:read").await?;
    let tenant = management_tenant(&service, query.tenant_external_id.clone())?;
    let filter = query.to_filter(true)?;
    let values = match tenant {
        Some(tenant) => state.db.list_all_requests_filtered(&tenant, filter).await?,
        None => state.db.list_global_requests_filtered(filter).await?,
    };
    Ok(Json(values))
}

pub(super) async fn internal_request_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(request_id): Path<Uuid>,
    Query(query): Query<ManagementTenantQuery>,
) -> Result<Response, AppError> {
    let service = require_service(&headers, &state, "requests:read").await?;
    let tenant = management_tenant(&service, query.tenant_external_id)?;
    let refs = match tenant {
        Some(tenant) => {
            state
                .db
                .request_archive_refs_for_tenant(&tenant, request_id)
                .await?
        }
        None => state.db.request_archive_refs_global(request_id).await?,
    };
    request_detail_response(&state, refs).await
}

pub(super) async fn internal_generation_asset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((job_id, asset_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<ManagementTenantQuery>,
) -> Result<Response, AppError> {
    let service = require_service(&headers, &state, "requests:read").await?;
    let requested_tenant = query.tenant_external_id.as_deref();
    let tenant = match service.tenant_external_id.as_deref() {
        Some(scoped) if requested_tenant.is_some_and(|requested| requested != scoped) => {
            return Err(AppError::NotFound);
        }
        Some(scoped) => Some(scoped),
        None => requested_tenant,
    };
    let asset = match tenant {
        Some(tenant) => {
            state
                .db
                .generation_asset_for_tenant(tenant, job_id, asset_id)
                .await?
        }
        None => state.db.generation_asset_global(job_id, asset_id).await?,
    };
    generation_asset_response(&state, &headers, asset).await
}

pub(super) async fn internal_request_asset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((request_id, asset_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<ManagementTenantQuery>,
) -> Result<Response, AppError> {
    let service = require_service(&headers, &state, "requests:read").await?;
    let requested_tenant = query.tenant_external_id.as_deref();
    let tenant = match service.tenant_external_id.as_deref() {
        Some(scoped) if requested_tenant.is_some_and(|requested| requested != scoped) => {
            return Err(AppError::NotFound);
        }
        Some(scoped) => Some(scoped),
        None => requested_tenant,
    };
    let asset = match tenant {
        Some(tenant) => {
            state
                .db
                .synchronous_generation_asset_for_tenant(tenant, request_id, asset_id)
                .await?
        }
        None => {
            state
                .db
                .synchronous_generation_asset_global(request_id, asset_id)
                .await?
        }
    };
    generation_asset_response(&state, &headers, asset).await
}

pub(super) async fn internal_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<StatsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "requests:read").await?;
    let tenant = management_tenant(&service, query.tenant_external_id.clone())?;
    let filter = query.to_filter(true, None)?;
    let stats = match tenant {
        Some(tenant) => state.db.operator_stats_filtered(&tenant, filter).await?,
        None => state.db.global_operator_stats_filtered(filter).await?,
    };
    Ok(Json(stats))
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ManagementTenantQuery {
    pub(super) tenant_external_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct RequestEventsQuery {
    after_event_at: Option<i64>,
    after_event_id: Option<Uuid>,
    tenant_external_id: Option<String>,
}

pub(super) async fn internal_request_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RequestEventsQuery>,
) -> Result<Response, AppError> {
    let service = require_service(&headers, &state, "requests:read").await?;
    let tenant = management_tenant(&service, query.tenant_external_id)?;
    let stream_permit = state
        .request_event_streams
        .try_acquire(service.service_id)
        .ok_or(AppError::RateLimited)?;
    let database = state.db.clone();
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(64);
    tokio::spawn(async move {
        let _stream_permit = stream_permit;
        let mut event_at = query
            .after_event_at
            .unwrap_or_else(|| unix_millis().saturating_sub(5_000));
        let mut event_id = query.after_event_id;
        loop {
            if sender.is_closed() {
                return;
            }
            let result = match tenant.as_deref() {
                Some(tenant) => {
                    database
                        .request_events_after(tenant, event_at, event_id, 500)
                        .await
                }
                None => {
                    database
                        .all_request_events_after(event_at, event_id, 500)
                        .await
                }
            };
            match result {
                Ok(events) => {
                    for request_event in events {
                        event_at = request_event.event_at;
                        event_id = Some(request_event.event_id);
                        let event = Event::default()
                            .id(request_event.event_id.to_string())
                            .event(format!("request.{}", request_event.event_kind))
                            .json_data(request_event);
                        let Ok(event) = event else {
                            continue;
                        };
                        if sender.send(Ok(event)).await.is_err() {
                            return;
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, tenant = ?tenant, "request event tail query failed");
                }
            }
            tokio::select! {
                () = sender.closed() => return,
                () = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
        }
    });
    Ok(Sse::new(ReceiverStream::new(receiver))
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response())
}
