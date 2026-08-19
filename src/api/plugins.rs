use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::Value;

use super::{management_tenant, require_service};
use crate::{
    AppState,
    error::AppError,
    plugin::{
        PluginConfigurationView, PutPluginConfigurationInput, plugin_configuration_request_hash,
        plugin_configuration_schema_digest,
    },
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct PluginConfigurationQuery {
    tenant_external_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct PutPluginConfigurationRequest {
    tenant_external_id: Option<String>,
    expected_version: i64,
    value: Value,
}

pub(in crate::api) async fn get_plugin_configuration(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(plugin_id): Path<String>,
    Query(query): Query<PluginConfigurationQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "plugins:read").await?;
    let tenant_external_id = management_tenant(&service, query.tenant_external_id)?;
    let tenant_id = match tenant_external_id.as_deref() {
        Some(tenant) => Some(state.db.plugin_configuration_tenant_id(tenant).await?),
        None => None,
    };
    Ok(Json(
        effective_configuration(&state, &plugin_id, tenant_external_id, tenant_id).await?,
    ))
}

pub(in crate::api) async fn put_plugin_configuration(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(plugin_id): Path<String>,
    Json(body): Json<PutPluginConfigurationRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "plugins:write").await?;
    let tenant_external_id = management_tenant(&service, body.tenant_external_id)?;
    let tenant_id = match tenant_external_id.as_deref() {
        Some(tenant) => Some(state.db.plugin_configuration_tenant_id(tenant).await?),
        None => None,
    };
    let contribution = state
        .plugins
        .configuration_contribution(&plugin_id)
        .ok_or(AppError::NotFound)?;
    crate::schema::validate_instance(&contribution.schema, &body.value)?;
    let schema_digest = plugin_configuration_schema_digest(&contribution.schema)?;
    let scope = tenant_id.map_or_else(|| "global".to_owned(), |id| format!("tenant:{id}"));
    let request_hash = plugin_configuration_request_hash(
        &plugin_id,
        &scope,
        body.expected_version,
        &schema_digest,
        &body.value,
    )?;
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 200
                && value.bytes().all(|byte| byte.is_ascii_graphic())
        })
        .ok_or_else(|| {
            AppError::BadRequest(
                "Idempotency-Key must contain 1 to 200 visible ASCII characters".into(),
            )
        })?
        .to_owned();
    let stored = state
        .db
        .put_plugin_configuration(PutPluginConfigurationInput {
            plugin_id: plugin_id.clone(),
            tenant_id,
            value: body.value,
            schema_digest: schema_digest.clone(),
            expected_version: body.expected_version,
            idempotency_key,
            request_hash,
        })
        .await?;
    state
        .plugins
        .invalidate_configuration_cache(stored.tenant_id)
        .await;
    Ok(Json(PluginConfigurationView {
        plugin_id,
        tenant_external_id,
        value: stored.value,
        source: if stored.tenant_id.is_some() {
            "tenant".into()
        } else {
            "global".into()
        },
        scope_version: stored.version,
        updated_at: Some(stored.updated_at),
        schema_digest,
    }))
}

async fn effective_configuration(
    state: &AppState,
    plugin_id: &str,
    tenant_external_id: Option<String>,
    tenant_id: Option<uuid::Uuid>,
) -> Result<PluginConfigurationView, AppError> {
    let contribution = state
        .plugins
        .configuration_contribution(plugin_id)
        .ok_or(AppError::NotFound)?;
    let direct = state
        .db
        .plugin_configuration_for_scope(plugin_id, tenant_id)
        .await?;
    let inherited = if tenant_id.is_some() && direct.is_none() {
        state
            .db
            .plugin_configuration_for_scope(plugin_id, None)
            .await?
    } else {
        None
    };
    let effective = direct.as_ref().or(inherited.as_ref());
    let value = effective
        .map(|stored| stored.value.clone())
        .unwrap_or_else(|| contribution.default.clone());
    crate::schema::validate_instance(&contribution.schema, &value)?;
    Ok(PluginConfigurationView {
        plugin_id: plugin_id.to_owned(),
        tenant_external_id,
        value,
        source: if direct.is_some() {
            if tenant_id.is_some() {
                "tenant"
            } else {
                "global"
            }
        } else if inherited.is_some() {
            "global"
        } else {
            "default"
        }
        .to_owned(),
        // Concurrency is always against the selected write scope. An inherited
        // global value therefore correctly starts a new tenant override at 0.
        scope_version: direct.as_ref().map_or(0, |stored| stored.version),
        updated_at: direct.as_ref().map(|stored| stored.updated_at),
        schema_digest: plugin_configuration_schema_digest(&contribution.schema)?,
    })
}
