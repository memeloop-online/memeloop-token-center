use std::{convert::Infallible, time::Duration};

use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::sse::{Event, KeepAlive, Sse},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use super::{
    RequestsQuery, StatsQuery, generation_asset_response, management_tenant,
    request_detail_response, require_global_service, require_service,
};
use crate::{
    AppState,
    db::unix_millis,
    error::AppError,
    filter_ast::TypedFilterAst,
    model::{AuthenticatedService, RequestListCursor, RequestListResponse},
};

const TYPED_FILTER_KV_NAMESPACE: &str = "typed-filter";
const MAX_FILTER_PRESETS: usize = 20;
const MAX_RECENT_FILTERS: usize = 8;
const MAX_ASSISTANT_PROMPT_BYTES: usize = 2_000;

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
    let limit = filter.limit.clamp(1, 500) as usize;
    let paged = query.requests_page_requested();
    // The database fetches one bounded extra row only for clients that opted
    // into the envelope. Existing clients retain their exact requested array
    // length and wire format.
    let mut query_filter = filter;
    query_filter.lookahead = paged;
    let values = match tenant {
        Some(tenant) => {
            state
                .db
                .list_all_requests_filtered(&tenant, query_filter)
                .await?
        }
        None => state.db.list_global_requests_filtered(query_filter).await?,
    };
    if !paged {
        return Ok(Json(serde_json::json!(values)));
    }
    let has_more = values.len() > limit;
    let mut requests = values;
    if has_more {
        requests.truncate(limit);
    }
    let next_cursor = has_more.then(|| {
        let last = requests
            .last()
            .expect("a page with another request has a visible last request");
        RequestListCursor {
            before_created_at: last.created_at,
            before_id: last.request_id,
        }
    });
    Ok(Json(serde_json::json!(RequestListResponse {
        requests,
        next_cursor,
    })))
}

/// POST is deliberately separate from the long-lived GET request-history
/// contract.  A typed AST can be sent without serializing JSON into a URL, and
/// older API clients retain the original array/envelope behavior unchanged.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TypedRequestQueryBody {
    pub tenant_external_id: Option<String>,
    #[serde(default = "default_control_list_limit")]
    pub limit: i64,
    #[serde(default)]
    pub paged: bool,
    pub before_created_at: Option<i64>,
    pub before_id: Option<Uuid>,
    pub ast: TypedFilterAst,
}

pub(super) async fn typed_internal_requests(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<TypedRequestQueryBody>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "requests:read").await?;
    let tenant = management_tenant(&service, body.tenant_external_id)?;
    body.ast.validate()?;
    let limit = body.limit.clamp(1, 100) as usize;
    let filter = crate::db::RequestListFilter {
        limit: limit as i64,
        lookahead: body.paged,
        before_created_at: body.before_created_at,
        before_id: body.before_id,
        typed_ast: Some(body.ast),
        ..crate::db::RequestListFilter::default()
    };
    let mut values = match tenant {
        Some(tenant) => state.db.list_all_requests_filtered(&tenant, filter).await?,
        None => state.db.list_global_requests_filtered(filter).await?,
    };
    if !body.paged {
        return Ok(Json(serde_json::json!(values)));
    }
    let has_more = values.len() > limit;
    if has_more {
        values.truncate(limit);
    }
    let next_cursor = has_more.then(|| {
        let last = values
            .last()
            .expect("a page with another request has a visible last request");
        RequestListCursor {
            before_created_at: last.created_at,
            before_id: last.request_id,
        }
    });
    Ok(Json(serde_json::json!(RequestListResponse {
        requests: values,
        next_cursor,
    })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FilterPresetQuery {
    tenant_external_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct NamedFilterPreset {
    pub name: String,
    pub ast: TypedFilterAst,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(super) struct FilterPresetState {
    pub named: Vec<NamedFilterPreset>,
    pub recent: Vec<TypedFilterAst>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PutFilterPresetBody {
    tenant_external_id: Option<String>,
    /// Omit `name` to record a bounded recent filter.  A name is a user-owned
    /// label, never a SQL identifier.
    name: Option<String>,
    ast: TypedFilterAst,
}

pub(super) async fn get_filter_presets(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<FilterPresetQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "requests:read").await?;
    let tenant = management_tenant(&service, query.tenant_external_id)?;
    Ok(Json(
        load_filter_preset_state(&state, &service, tenant.as_deref()).await?,
    ))
}

pub(super) async fn put_filter_preset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PutFilterPresetBody>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "requests:read").await?;
    let tenant = management_tenant(&service, body.tenant_external_id)?;
    body.ast.validate()?;
    let mut stored = load_filter_preset_state(&state, &service, tenant.as_deref()).await?;
    if let Some(name) = body.name {
        let name = validate_filter_preset_name(name)?;
        stored.named.retain(|preset| preset.name != name);
        stored.named.insert(
            0,
            NamedFilterPreset {
                name,
                ast: body.ast.clone(),
                updated_at: unix_millis(),
            },
        );
        stored.named.truncate(MAX_FILTER_PRESETS);
    }
    stored.recent.retain(|ast| ast != &body.ast);
    stored.recent.insert(0, body.ast);
    stored.recent.truncate(MAX_RECENT_FILTERS);
    store_filter_preset_state(&state, &service, tenant.as_deref(), &stored).await?;
    Ok(Json(stored))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct FilterAssistantSettings {
    /// The only persisted execution reference.  The associated upstream
    /// credential remains encrypted and is never serialized by this API.
    pub model_route_id: Uuid,
    #[serde(default)]
    pub billing_key_id: Option<Uuid>,
    pub updated_at: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FilterAssistantSettingsQuery {
    tenant_external_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FilterAssistantBillingQuery {
    tenant_external_id: Option<String>,
    model_route_id: Uuid,
}

pub(super) async fn filter_assistant_billing_choices(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<FilterAssistantBillingQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "keys:read").await?;
    let tenant = required_filter_tenant(&service, query.tenant_external_id)?;
    state
        .db
        .filter_assistant_route(&tenant, query.model_route_id)
        .await?;
    let keys = state
        .db
        .list_managed_keys_page(Some(&tenant), None, None, 100, None)
        .await?;
    let mut choices = Vec::new();
    for key in keys {
        if key.status == "active"
            && assistant_execution_context(&state, &tenant, query.model_route_id, key.key_id)
                .await
                .is_ok()
        {
            choices.push(json!({"key_id": key.key_id, "alias": key.alias, "principal": key.principal_external_id}));
        }
    }
    Ok(Json(choices))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PutFilterAssistantSettingsBody {
    tenant_external_id: Option<String>,
    model_route_id: Uuid,
    billing_key_id: Uuid,
    expected_updated_at: Option<i64>,
}

pub(super) async fn get_filter_assistant_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<FilterAssistantSettingsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "requests:read").await?;
    let tenant = required_filter_tenant(&service, query.tenant_external_id)?;
    Ok(Json(load_filter_assistant_settings(&state, &tenant).await?))
}

pub(super) async fn put_filter_assistant_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PutFilterAssistantSettingsBody>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:write").await?;
    // An assistant route is a system policy: scoped operator credentials can
    // consume it but cannot redirect it to another model route.
    require_global_service(&service)?;
    let tenant = required_filter_tenant(&service, body.tenant_external_id)?;
    require_service(&headers, &state, "keys:write").await?;
    let previous = load_filter_assistant_settings(&state, &tenant).await?;
    if previous.as_ref().map(|value| value.updated_at) != body.expected_updated_at {
        return Err(AppError::Conflict(
            "filter assistant settings changed; reload before saving".into(),
        ));
    }
    assistant_execution_context(&state, &tenant, body.model_route_id, body.billing_key_id).await?;
    let settings = FilterAssistantSettings {
        model_route_id: body.model_route_id,
        billing_key_id: Some(body.billing_key_id),
        updated_at: unix_millis().max(body.expected_updated_at.unwrap_or(0).saturating_add(1)),
    };
    let storage_key = filter_assistant_settings_storage_key(&tenant);
    let expected = state
        .db
        .plugin_kv_get(TYPED_FILTER_KV_NAMESPACE, &storage_key)
        .await?;
    // Match the exact stored generation as well as its timestamp.
    if expected
        .as_deref()
        .map(serde_json::from_slice::<FilterAssistantSettings>)
        .transpose()
        .map_err(|_| AppError::Internal)?
        .map(|value| value.updated_at)
        != body.expected_updated_at
    {
        return Err(AppError::Conflict(
            "filter assistant settings changed; reload before saving".into(),
        ));
    }
    let value = serde_json::to_vec(&settings).map_err(|_| AppError::Internal)?;
    state
        .db
        .replace_filter_assistant_settings(
            &storage_key,
            expected.as_deref(),
            &value,
            service.service_id,
        )
        .await?;
    Ok(Json(Some(settings)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FilterAssistantPlanBody {
    tenant_external_id: Option<String>,
    prompt: String,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct FilterAssistantPlan {
    pub model_route_id: Uuid,
    pub ast: TypedFilterAst,
}

pub(super) async fn plan_filter_with_assistant(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<FilterAssistantPlanBody>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "requests:read").await?;
    let tenant = required_filter_tenant(&service, body.tenant_external_id)?;
    validate_assistant_prompt(&body.prompt)?;
    let settings = load_filter_assistant_settings(&state, &tenant)
        .await?
        .ok_or_else(|| AppError::BadRequest("filter assistant is not configured".into()))?;
    let billing_key_id = settings.billing_key_id.ok_or_else(|| AppError::BadRequest("filter assistant model execution is not enabled; configure a billing credential in System settings".into()))?;
    let (key, model, protocol) =
        assistant_execution_context(&state, &tenant, settings.model_route_id, billing_key_id)
            .await?;
    tracing::info!(actor_service_id = ?service.service_id, %billing_key_id, model_route_id = %settings.model_route_id, "filter assistant model execution admitted");
    let ast = super::filter_assistant::execute(
        state,
        key,
        settings.model_route_id,
        &model,
        &protocol,
        &body.prompt,
    )
    .await?;
    Ok(Json(FilterAssistantPlan {
        model_route_id: settings.model_route_id,
        ast,
    }))
}

fn required_filter_tenant(
    service: &AuthenticatedService,
    requested: Option<String>,
) -> Result<String, AppError> {
    management_tenant(service, requested)?.ok_or_else(|| {
        AppError::BadRequest("tenant_external_id is required for filter assistant settings".into())
    })
}

async fn load_filter_preset_state(
    state: &AppState,
    service: &AuthenticatedService,
    tenant: Option<&str>,
) -> Result<FilterPresetState, AppError> {
    let Some(value) = state
        .db
        .plugin_kv_get(
            TYPED_FILTER_KV_NAMESPACE,
            &filter_preset_storage_key(service, tenant),
        )
        .await?
    else {
        return Ok(FilterPresetState::default());
    };
    serde_json::from_slice(&value).map_err(|_| AppError::Internal)
}

async fn store_filter_preset_state(
    state: &AppState,
    service: &AuthenticatedService,
    tenant: Option<&str>,
    stored: &FilterPresetState,
) -> Result<(), AppError> {
    let value = serde_json::to_vec(stored).map_err(|_| AppError::Internal)?;
    state
        .db
        .plugin_kv_put(
            TYPED_FILTER_KV_NAMESPACE,
            &filter_preset_storage_key(service, tenant),
            &value,
        )
        .await
}

async fn load_filter_assistant_settings(
    state: &AppState,
    tenant: &str,
) -> Result<Option<FilterAssistantSettings>, AppError> {
    let Some(value) = state
        .db
        .plugin_kv_get(
            TYPED_FILTER_KV_NAMESPACE,
            &filter_assistant_settings_storage_key(tenant),
        )
        .await?
    else {
        return Ok(None);
    };
    serde_json::from_slice(&value)
        .map(Some)
        .map_err(|_| AppError::Internal)
}

async fn assistant_execution_context(
    state: &AppState,
    tenant: &str,
    route_id: Uuid,
    billing_key_id: Uuid,
) -> Result<(crate::model::AuthenticatedKey, String, String), AppError> {
    let (model, protocol) = state.db.filter_assistant_route(tenant, route_id).await?;
    let key = state
        .db
        .filter_assistant_identity(tenant, billing_key_id)
        .await?;
    let candidates = state
        .db
        .list_authorized_upstream_candidates_with_hint(
            key.key_id,
            key.tenant_id,
            &model,
            &protocol,
            crate::db::RouteSelectionOptions {
                upstream_account_hint: None,
                selection_seed: Uuid::now_v7(),
            },
        )
        .await?;
    if !candidates.iter().any(|candidate| {
        candidate.route_id == route_id
            && state
                .providers
                .get(&candidate.driver)
                .is_some_and(|provider| {
                    provider
                        .modalities
                        .iter()
                        .any(|modality| modality == "text")
                })
    }) {
        return Err(AppError::BadRequest(
            "billing credential has no available text account on the selected route".into(),
        ));
    }
    Ok((key, model, protocol))
}

fn filter_preset_storage_key(service: &AuthenticatedService, tenant: Option<&str>) -> String {
    let identity = service
        .service_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "bootstrap".to_owned());
    format!(
        "presets/{identity}/{}",
        storage_digest(tenant.unwrap_or("all"))
    )
}

fn filter_assistant_settings_storage_key(tenant: &str) -> String {
    format!("settings/{}", storage_digest(tenant))
}

fn storage_digest(value: &str) -> String {
    let digest = blake3::hash(value.as_bytes()).to_hex().to_string();
    digest[..32].to_owned()
}

fn validate_filter_preset_name(name: String) -> Result<String, AppError> {
    let name = name.trim().to_owned();
    if name.is_empty() || name.len() > 80 || name.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "filter preset name must contain 1 to 80 non-control characters".into(),
        ));
    }
    Ok(name)
}

fn validate_assistant_prompt(prompt: &str) -> Result<(), AppError> {
    if prompt.trim().is_empty()
        || prompt.len() > MAX_ASSISTANT_PROMPT_BYTES
        || prompt.chars().any(char::is_control)
    {
        return Err(AppError::BadRequest(format!(
            "assistant prompt must contain 1 to {MAX_ASSISTANT_PROMPT_BYTES} non-control characters"
        )));
    }
    Ok(())
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

#[derive(Debug, Deserialize)]
pub(super) struct GenerationJobsQuery {
    tenant_external_id: Option<String>,
    #[serde(default = "default_control_list_limit")]
    limit: i64,
}

pub(super) async fn internal_generations(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<GenerationJobsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "requests:read").await?;
    let tenant = management_tenant(&service, query.tenant_external_id)?;
    Ok(Json(
        state
            .db
            .operator_generation_jobs(tenant.as_deref(), query.limit)
            .await?,
    ))
}

pub(super) async fn internal_generation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(job_id): Path<Uuid>,
    Query(query): Query<ManagementTenantQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "requests:read").await?;
    let tenant = management_tenant(&service, query.tenant_external_id)?;
    Ok(Json(
        state
            .db
            .operator_generation_job(tenant.as_deref(), job_id)
            .await?,
    ))
}

pub(super) async fn cancel_internal_generation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(job_id): Path<Uuid>,
    Query(query): Query<ManagementTenantQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "generations:write").await?;
    let tenant = management_tenant(&service, query.tenant_external_id)?.ok_or_else(|| {
        AppError::BadRequest("tenant_external_id is required when cancelling a generation".into())
    })?;
    let scoped = state
        .db
        .operator_generation_job(Some(&tenant), job_id)
        .await?;
    let job = state
        .db
        .cancel_generation_job(scoped.key_id, job_id)
        .await?;
    Ok(Json(crate::model::OperatorGenerationJobView {
        job,
        tenant_external_id: scoped.tenant_external_id,
        key_id: scoped.key_id,
        key_alias: scoped.key_alias,
        currency: scoped.currency,
    }))
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
    let stream_activity = state
        .metrics
        .active_stream(crate::metrics::ActiveStreamKind::RequestEvents);
    let database = state.db.clone();
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(64);
    tokio::spawn(async move {
        let _stream_permit = stream_permit;
        let _stream_activity = stream_activity;
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
