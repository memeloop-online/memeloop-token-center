use std::{collections::BTreeMap, time::Duration};

use futures_util::StreamExt;

use super::super::*;
use crate::db::{DiscoveredUpstreamModel, ReplaceModelCatalogResult, UpstreamModelCatalogView};

const MODEL_CATALOG_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_MODEL_CATALOG_BODY: usize = 2 * 1024 * 1024;
const MAX_MODEL_COUNT: usize = 10_000;
const MAX_MODEL_ID_BYTES: usize = 500;

#[derive(Debug, Deserialize)]
pub(in crate::api) struct UpstreamModelsQuery {
    tenant_external_id: Option<String>,
    q: Option<String>,
    #[serde(default = "default_model_limit")]
    limit: i64,
}

#[derive(Debug, Deserialize)]
pub(in crate::api) struct SyncUpstreamModelsQuery {
    tenant_external_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(in crate::api) struct AggregateUpstreamModelsQuery {
    tenant_external_id: Option<String>,
    account_ids: Option<String>,
    include_provider_group_ids: Option<String>,
    exclude_provider_group_ids: Option<String>,
    q: Option<String>,
    #[serde(default = "default_model_limit")]
    limit: i64,
}

fn default_model_limit() -> i64 {
    100
}

pub(in crate::api) async fn list_upstream_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Query(query): Query<UpstreamModelsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service_any(&headers, &state, &["providers:read", "routes:read"]).await?;
    let tenant = account_tenant(&state, &service, account_id, query.tenant_external_id).await?;
    if query
        .q
        .as_ref()
        .is_some_and(|value| value.len() > MAX_MODEL_ID_BYTES)
    {
        return Err(AppError::BadRequest(
            "model search contains too many bytes".into(),
        ));
    }
    Ok(Json(
        state
            .db
            .upstream_model_catalog(account_id, &tenant, query.q.as_deref(), query.limit)
            .await?,
    ))
}

pub(in crate::api) async fn aggregate_upstream_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AggregateUpstreamModelsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service_any(&headers, &state, &["providers:read", "routes:read"]).await?;
    let tenant = management_tenant(&service, query.tenant_external_id)?.ok_or_else(|| {
        AppError::BadRequest("tenant_external_id is required for a global service".into())
    })?;
    if query
        .q
        .as_ref()
        .is_some_and(|value| value.len() > MAX_MODEL_ID_BYTES)
    {
        return Err(AppError::BadRequest(
            "model search contains too many bytes".into(),
        ));
    }
    let explicit = parse_uuid_list(query.account_ids.as_deref())?;
    let included = parse_uuid_list(query.include_provider_group_ids.as_deref())?;
    let excluded = parse_uuid_list(query.exclude_provider_group_ids.as_deref())?;
    Ok(Json(
        state
            .db
            .aggregate_upstream_models(
                &tenant,
                &explicit,
                &included,
                &excluded,
                query.q.as_deref(),
                query.limit,
            )
            .await?,
    ))
}

fn parse_uuid_list(value: Option<&str>) -> Result<Vec<Uuid>, AppError> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    let values = value
        .split(',')
        .map(|value| {
            value
                .parse::<Uuid>()
                .map_err(|_| AppError::BadRequest("invalid model catalog selection ID".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if values.len() > 100 {
        return Err(AppError::BadRequest(
            "model catalog selection is too large".into(),
        ));
    }
    Ok(values)
}

pub(in crate::api) async fn sync_upstream_models(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Query(query): Query<SyncUpstreamModelsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:write").await?;
    let tenant = account_tenant(&state, &service, account_id, query.tenant_external_id).await?;
    Ok(Json(
        sync_account_models(&state, account_id, &tenant).await?,
    ))
}

pub(crate) fn trigger_upstream_model_sync(state: AppState, account_id: Uuid) {
    tokio::spawn(async move {
        let Ok((account, _)) = state
            .db
            .upstream_account_with_credential(account_id, state.config.key_pepper.as_bytes())
            .await
        else {
            return;
        };
        let Some(tenant) = account.tenant_external_id.as_deref() else {
            return;
        };
        let _ = sync_account_models(&state, account_id, tenant).await;
    });
}

async fn account_tenant(
    state: &AppState,
    service: &AuthenticatedService,
    account_id: Uuid,
    requested: Option<String>,
) -> Result<String, AppError> {
    let tenant = state
        .db
        .upstream_account_tenant_external_id(account_id)
        .await?;
    if let Some(requested) = requested {
        require_service_tenant(service, &requested)?;
        if requested != tenant {
            return Err(AppError::NotFound);
        }
    }
    require_service_tenant(service, &tenant)?;
    Ok(tenant)
}

async fn sync_account_models(
    state: &AppState,
    account_id: Uuid,
    tenant_external_id: &str,
) -> Result<UpstreamModelCatalogView, AppError> {
    let (account, credential) = state
        .db
        .upstream_account_with_credential(account_id, state.config.key_pepper.as_bytes())
        .await?;
    if account.tenant_external_id.as_deref() != Some(tenant_external_id) {
        return Err(AppError::NotFound);
    }
    let generation = account.credential_generation;
    let lease_id = Uuid::now_v7();
    if !state
        .db
        .claim_upstream_model_catalog_sync(account_id, tenant_external_id, generation, lease_id)
        .await?
    {
        return state
            .db
            .upstream_model_catalog(account_id, tenant_external_id, None, 100)
            .await;
    }
    let discovery = discover_models(state, &account, &credential).await;
    match discovery {
        Ok((source_kind, models)) => {
            let replaced = state
                .db
                .replace_upstream_model_catalog(
                    account_id,
                    tenant_external_id,
                    generation,
                    lease_id,
                    source_kind,
                    &models,
                )
                .await?;
            if replaced != ReplaceModelCatalogResult::Replaced {
                return Err(AppError::Conflict(
                    "upstream credential changed while models were synchronizing".into(),
                ));
            }
        }
        Err(code) => {
            let replaced = state
                .db
                .record_upstream_model_catalog_failure(
                    account_id,
                    tenant_external_id,
                    generation,
                    lease_id,
                    code,
                )
                .await?;
            if replaced != ReplaceModelCatalogResult::Replaced {
                return Err(AppError::Conflict(
                    "upstream credential changed while models were synchronizing".into(),
                ));
            }
        }
    }
    state
        .db
        .upstream_model_catalog(account_id, tenant_external_id, None, 100)
        .await
}

async fn discover_models(
    state: &AppState,
    account: &crate::provider::UpstreamAccountView,
    credential: &UpstreamCredential,
) -> Result<(&'static str, Vec<DiscoveredUpstreamModel>), &'static str> {
    let plugins = state.plugins.clone();
    let driver = account.driver.clone();
    let config = account.config.clone();
    let plugin_result =
        tokio::task::spawn_blocking(move || plugins.list_provider_models(&driver, &config))
            .await
            .map_err(|_| "upstream_unavailable")?
            .map_err(|_| "upstream_unavailable")?;
    if let Some(value) = plugin_result {
        return parse_model_array(&value).map(|models| ("component", models));
    }
    if matches!(account.driver.as_str(), "openai-codex" | "cpa-codex-oauth") {
        return discover_codex_models(state, account, credential).await;
    }
    if account.driver != "http-json" {
        return Err("unsupported");
    }
    credential
        .validate(unix_millis())
        .map_err(|_| "credential_invalid")?;
    let base_url = validate_config(&account.config).map_err(|_| "destination_invalid")?;
    let client = network::client_for_config_url(
        &state.http,
        &base_url,
        &account.config,
        state.config.allow_oauth_loopback,
    )
    .await
    .map_err(|_| "destination_invalid")?;
    let url = if base_url.ends_with("/v1") {
        format!("{base_url}/models")
    } else {
        format!("{base_url}/v1/models")
    };
    let request = credential
        .apply(
            client
                .get(url)
                .header(header::ACCEPT, "application/json")
                .timeout(MODEL_CATALOG_TIMEOUT),
            unix_millis(),
        )
        .map_err(|_| "credential_invalid")?;
    let response = request.send().await.map_err(|_| "connection_failed")?;
    let status = response.status();
    if status.is_redirection() {
        return Err("redirect_rejected");
    }
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err("authentication_failed");
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err("rate_limited");
    }
    if !status.is_success() {
        return Err("upstream_unavailable");
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MODEL_CATALOG_BODY as u64)
    {
        return Err("response_too_large");
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "connection_failed")?;
        if body.len().saturating_add(chunk.len()) > MAX_MODEL_CATALOG_BODY {
            return Err("response_too_large");
        }
        body.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&body).map_err(|_| "invalid_response")?;
    let data = value.get("data").ok_or("invalid_response")?;
    parse_model_array(data).map(|models| ("openai_v1", models))
}

async fn discover_codex_models(
    state: &AppState,
    account: &crate::provider::UpstreamAccountView,
    credential: &UpstreamCredential,
) -> Result<(&'static str, Vec<DiscoveredUpstreamModel>), &'static str> {
    credential
        .validate(unix_millis())
        .map_err(|_| "credential_invalid")?;
    let base_url = validate_config(&account.config).map_err(|_| "destination_invalid")?;
    let client = network::client_for_config_url(
        &state.http,
        &base_url,
        &account.config,
        state.config.allow_oauth_loopback,
    )
    .await
    .map_err(|_| "destination_invalid")?;
    let account_id = codex_account_header(credential)?;
    let url = format!(
        "{}/models?client_version={}",
        base_url.trim_end_matches('/'),
        env!("CARGO_PKG_VERSION")
    );
    let request = credential
        .apply(
            client
                .get(url)
                .header(header::ACCEPT, "application/json")
                .header(
                    header::USER_AGENT,
                    concat!("memeloop-token-center/", env!("CARGO_PKG_VERSION")),
                )
                .header("originator", "memeloop-token-center")
                .header("chatgpt-account-id", account_id)
                .timeout(MODEL_CATALOG_TIMEOUT),
            unix_millis(),
        )
        .map_err(|_| "credential_invalid")?;
    let value = bounded_json_response(request).await?;
    let values = value
        .get("models")
        .and_then(Value::as_array)
        .ok_or("invalid_response")?;
    if values.len() > MAX_MODEL_COUNT {
        return Err("invalid_response");
    }
    let normalized = values
        .iter()
        .filter(|value| {
            value
                .get("supported_in_api")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && value.get("visibility").and_then(Value::as_str) == Some("list")
        })
        .map(|value| {
            let id = value
                .get("slug")
                .and_then(Value::as_str)
                .ok_or("invalid_response")?;
            validate_model_id(id)?;
            let context_window = value
                .get("context_window")
                .and_then(Value::as_i64)
                .filter(|limit| (1..=10_000_000).contains(limit))
                .ok_or("invalid_response")?;
            Ok(DiscoveredUpstreamModel {
                model_id: id.to_owned(),
                protocol: "openai".to_owned(),
                context_window: Some(context_window),
                // Codex does not publish an output maximum. The authenticated
                // total context window is stored as a conservative reservation
                // bound because output cannot exceed total context.
                reservation_token_bound: Some(context_window),
                reservation_bound_source: Some("mtc_context_window_bound".to_owned()),
            })
        })
        .collect::<Result<Vec<_>, &'static str>>()?;
    parse_discovered_models(normalized).map(|models| ("codex_models", models))
}

fn codex_account_header(credential: &UpstreamCredential) -> Result<String, &'static str> {
    let Some(state) = credential.adapter_state().and_then(Value::as_object) else {
        return Err("credential_invalid");
    };
    if state.len() != 2
        || !matches!(
            state.get("schema").and_then(Value::as_str),
            Some("openai-codex-oauth-v1" | "cpa-codex-oauth-v1")
        )
    {
        return Err("credential_invalid");
    }
    let account_id = state
        .get("account_id")
        .and_then(Value::as_str)
        .ok_or("credential_invalid")?;
    if account_id.is_empty()
        || account_id.len() > 200
        || !account_id.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    {
        return Err("credential_invalid");
    }
    Ok(account_id.to_owned())
}

async fn bounded_json_response(request: reqwest::RequestBuilder) -> Result<Value, &'static str> {
    let response = request.send().await.map_err(|_| "connection_failed")?;
    let status = response.status();
    if status.is_redirection() {
        return Err("redirect_rejected");
    }
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err("authentication_failed");
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err("rate_limited");
    }
    if !status.is_success() {
        return Err("upstream_unavailable");
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MODEL_CATALOG_BODY as u64)
    {
        return Err("response_too_large");
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "connection_failed")?;
        if body.len().saturating_add(chunk.len()) > MAX_MODEL_CATALOG_BODY {
            return Err("response_too_large");
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| "invalid_response")
}

fn parse_model_array(value: &Value) -> Result<Vec<DiscoveredUpstreamModel>, &'static str> {
    if serde_json::to_vec(value)
        .map_err(|_| "invalid_response")?
        .len()
        > MAX_MODEL_CATALOG_BODY
    {
        return Err("response_too_large");
    }
    let values = value.as_array().ok_or("invalid_response")?;
    if values.len() > MAX_MODEL_COUNT {
        return Err("invalid_response");
    }
    let mut parsed = BTreeMap::<(String, String), Option<i64>>::new();
    for value in values {
        let (id, protocol, context_window) = match value {
            Value::String(id) => (id.as_str(), "any", None),
            Value::Object(object) => {
                let id = object
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("invalid_response")?;
                let protocol = object
                    .get("protocol")
                    .and_then(Value::as_str)
                    .unwrap_or("any");
                let context_window = object.get("context_window").and_then(Value::as_i64);
                (id, protocol, context_window)
            }
            _ => return Err("invalid_response"),
        };
        validate_model_id(id)?;
        if protocol.is_empty()
            || protocol.len() > 64
            || !protocol.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'-' | b'_' | b'.')
            })
            || context_window.is_some_and(|limit| !(1..=10_000_000).contains(&limit))
        {
            return Err("invalid_response");
        }
        let key = (id.to_owned(), protocol.to_owned());
        if parsed
            .insert(key, context_window)
            .is_some_and(|previous| previous != context_window)
        {
            return Err("invalid_response");
        }
    }
    Ok(parsed
        .into_iter()
        .map(
            |((model_id, protocol), context_window)| DiscoveredUpstreamModel {
                model_id,
                protocol,
                context_window,
                reservation_token_bound: None,
                reservation_bound_source: None,
            },
        )
        .collect())
}

fn parse_discovered_models(
    models: Vec<DiscoveredUpstreamModel>,
) -> Result<Vec<DiscoveredUpstreamModel>, &'static str> {
    if models.len() > MAX_MODEL_COUNT {
        return Err("invalid_response");
    }
    let mut parsed = BTreeMap::new();
    for model in models {
        validate_model_id(&model.model_id)?;
        if model.protocol.is_empty()
            || model.protocol.len() > 64
            || !model.protocol.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'-' | b'_' | b'.')
            })
            || model
                .context_window
                .is_some_and(|limit| !(1..=10_000_000).contains(&limit))
            || model
                .reservation_token_bound
                .is_some_and(|limit| !(1..=10_000_000).contains(&limit))
            || model
                .reservation_bound_source
                .as_deref()
                .is_some_and(|source| {
                    source != "mtc_context_window_bound" && source != "administrator_override"
                })
        {
            return Err("invalid_response");
        }
        let key = (model.model_id.clone(), model.protocol.clone());
        if parsed.insert(key, model).is_some() {
            return Err("invalid_response");
        }
    }
    Ok(parsed.into_values().collect())
}

fn validate_model_id(id: &str) -> Result<(), &'static str> {
    if id.is_empty()
        || id.len() > MAX_MODEL_ID_BYTES
        || id.trim() != id
        || id.chars().any(char::is_control)
    {
        return Err("invalid_response");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_parser_is_bounded_deduplicated_and_rejects_control_characters() {
        let models = parse_model_array(&json!([
            {"id": "gpt-5", "context_window": 8192},
            {"id": "gpt-5", "context_window": 8192},
            "text-embedding-3-small"
        ]))
        .unwrap();
        assert_eq!(models.len(), 2);
        assert!(parse_model_array(&json!([{"id": "bad\nmodel"}])).is_err());
        assert!(parse_model_array(&json!([{"id": "gpt", "protocol": "UPPER"}])).is_err());
        assert!(
            parse_model_array(&json!([
                {"id": "gpt", "context_window": 1},
                {"id": "gpt", "context_window": 2}
            ]))
            .is_err()
        );
    }
}
