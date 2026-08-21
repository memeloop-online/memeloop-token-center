use std::{collections::BTreeMap, time::Duration};

use axum::{body::Bytes, extract::State, http::HeaderMap, response::Response};
use serde_json::Value;
use uuid::Uuid;

use super::{MAX_IMAGE_REQUEST_BODY, MAX_REPORTED_TOKENS};
use crate::{
    AppState,
    error::AppError,
    model::AuthenticatedKey,
    network,
    plugin::{
        NormalizedProviderResponse, PreparedProviderRequest,
        memeloop::token_center::types::RequestContext,
    },
};

static PLUGIN_EXECUTION_PERMITS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);

pub(super) async fn proxy_openai_chat(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    super::proxy::proxy(state, headers, body, Protocol::OpenAiChat).await
}

pub(super) async fn proxy_openai_responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    super::proxy::proxy(state, headers, body, Protocol::OpenAiResponses).await
}

pub(super) async fn proxy_openai_embeddings(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    super::proxy::proxy(state, headers, body, Protocol::OpenAiEmbeddings).await
}

pub(super) async fn proxy_anthropic(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    super::proxy::proxy(state, headers, body, Protocol::AnthropicMessages).await
}

pub(super) async fn proxy_anthropic_count_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    super::proxy::proxy(state, headers, body, Protocol::AnthropicCountTokens).await
}

#[derive(Clone, Copy)]
pub(super) enum Protocol {
    OpenAiChat,
    OpenAiResponses,
    OpenAiEmbeddings,
    AnthropicMessages,
    AnthropicCountTokens,
}

impl Protocol {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::OpenAiChat | Self::OpenAiResponses | Self::OpenAiEmbeddings => "openai",
            Self::AnthropicMessages | Self::AnthropicCountTokens => "anthropic",
        }
    }

    pub(super) fn path(self) -> &'static str {
        match self {
            Self::OpenAiChat => "/v1/chat/completions",
            Self::OpenAiResponses => "/v1/responses",
            Self::OpenAiEmbeddings => "/v1/embeddings",
            Self::AnthropicMessages => "/v1/messages",
            Self::AnthropicCountTokens => "/v1/messages/count_tokens",
        }
    }
}

pub(super) fn inject_controlled_output_ceiling(
    protocol: Protocol,
    request: &mut Value,
) -> Result<i64, AppError> {
    let object = request
        .as_object_mut()
        .ok_or_else(|| AppError::BadRequest("request body must be a JSON object".to_owned()))?;
    let (accepted_fields, injected_field, default) = match protocol {
        Protocol::OpenAiChat => (
            &["max_completion_tokens", "max_tokens"][..],
            Some("max_completion_tokens"),
            4_096,
        ),
        Protocol::OpenAiResponses => (&["max_output_tokens"][..], Some("max_output_tokens"), 4_096),
        Protocol::AnthropicMessages => (&["max_tokens"][..], Some("max_tokens"), 4_096),
        Protocol::OpenAiEmbeddings | Protocol::AnthropicCountTokens => (&[][..], None, 0),
    };
    if matches!(protocol, Protocol::OpenAiChat)
        && object.contains_key("max_completion_tokens")
        && object.contains_key("max_tokens")
    {
        return Err(AppError::BadRequest(
            "max_completion_tokens and max_tokens cannot be supplied together".to_owned(),
        ));
    }
    for field in accepted_fields {
        if let Some(value) = object.get(*field) {
            let ceiling = value.as_i64().ok_or_else(|| {
                AppError::BadRequest(format!("{field} must be a non-negative integer"))
            })?;
            if !(0..=MAX_REPORTED_TOKENS).contains(&ceiling) {
                return Err(AppError::BadRequest(format!(
                    "{field} must be between 0 and {MAX_REPORTED_TOKENS}"
                )));
            }
            return Ok(ceiling);
        }
    }
    if let Some(field) = injected_field {
        object.insert(field.to_owned(), Value::from(default));
    }
    Ok(default)
}

pub(super) struct AppliedTraffic {
    pub(super) request_json: Value,
    pub(super) model: String,
    pub(super) upstream_account_hint: Option<Uuid>,
}

/// Traffic plugins run only after core key authentication. Both the client
/// model and the effective rewritten model must resolve through normalized
/// exact-route or route-group grants. A plugin may narrow the resulting
/// account candidates, but cannot create a permission or bypass exclusions.
pub(super) async fn apply_traffic_policy(
    state: &AppState,
    key: &AuthenticatedKey,
    protocol: &str,
    original_request_json: Value,
) -> Result<AppliedTraffic, AppError> {
    if !original_request_json.is_object() {
        return Err(AppError::BadRequest(
            "request body must be a JSON object".into(),
        ));
    }
    let requested_model = original_request_json
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty() && value.len() <= 200)
        .ok_or_else(|| AppError::BadRequest("model is required".into()))?
        .to_owned();
    if !state
        .db
        .credential_has_available_route(key.key_id, key.tenant_id, &requested_model, protocol)
        .await?
    {
        return Err(AppError::Forbidden);
    }
    let plugins = state.plugins.clone();
    let plugin_configurations = plugins
        .resolved_traffic_configurations(key.tenant_id)
        .await?;
    let plugin_request = original_request_json.clone();
    let plugin_context = RequestContext {
        tenant_id: key.tenant_id.to_string(),
        principal_id: key.principal_id.to_string(),
        key_id: key.key_id.to_string(),
        protocol: protocol.to_owned(),
        model: requested_model.clone(),
        config_json: "{}".to_owned(),
    };
    let plugin_permit =
        tokio::time::timeout(Duration::from_secs(1), PLUGIN_EXECUTION_PERMITS.acquire())
            .await
            .map_err(|_| AppError::Upstream("plugin execution capacity is exhausted".into()))?
            .map_err(|_| AppError::Internal)?;
    let plugin_task = tokio::task::spawn_blocking(move || {
        let _plugin_permit = plugin_permit;
        plugins.apply_traffic_with_config(plugin_context, &plugin_request, &plugin_configurations)
    });
    let plugin_decision = tokio::time::timeout(Duration::from_secs(35), plugin_task)
        .await
        .map_err(|_| AppError::Upstream("plugin execution timed out".into()))?
        .map_err(|error| AppError::Upstream(format!("plugin task failed: {error}")))??;
    if !plugin_decision.allow {
        plugin_decision.log_denial();
        return Err(AppError::Forbidden);
    }
    let mut request_json = plugin_decision
        .request_json
        .unwrap_or(original_request_json);
    if !request_json.is_object() {
        return Err(AppError::Upstream(
            "plugin returned a request that is not a JSON object".into(),
        ));
    }
    let rewritten_model = request_json
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty() && value.len() <= 200)
        .map(str::to_owned);
    let model = plugin_decision
        .model
        .or(rewritten_model)
        .unwrap_or(requested_model);
    if model.trim().is_empty() || model.len() > 200 {
        return Err(AppError::Upstream(
            "plugin returned an invalid model".into(),
        ));
    }
    if !state
        .db
        .credential_has_available_route(key.key_id, key.tenant_id, &model, protocol)
        .await?
    {
        return Err(AppError::Forbidden);
    }
    request_json["model"] = Value::String(model.clone());
    if serde_json::to_vec(&request_json)
        .map_err(|_| AppError::Internal)?
        .len()
        > MAX_IMAGE_REQUEST_BODY
    {
        return Err(AppError::Upstream(
            "plugin-rewritten request exceeds 16 MiB".into(),
        ));
    }
    let upstream_account_hint = plugin_decision
        .upstream_account_id
        .map(|value| {
            Uuid::parse_str(&value).map_err(|_| {
                AppError::Upstream("plugin returned an invalid upstream account id".into())
            })
        })
        .transpose()?;
    Ok(AppliedTraffic {
        request_json,
        model,
        upstream_account_hint,
    })
}

pub(super) async fn prepare_component_provider(
    state: &AppState,
    provider_id: &str,
    context: RequestContext,
    config: Value,
    request_json: Value,
) -> Result<PreparedProviderRequest, AppError> {
    let plugins = state.plugins.clone();
    let provider_id = provider_id.to_owned();
    let permit = tokio::time::timeout(Duration::from_secs(1), PLUGIN_EXECUTION_PERMITS.acquire())
        .await
        .map_err(|_| AppError::Upstream("plugin execution capacity is exhausted".into()))?
        .map_err(|_| AppError::Internal)?;
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        plugins.prepare_provider_request(&provider_id, context, &config, &request_json)
    });
    tokio::time::timeout(Duration::from_secs(35), task)
        .await
        .map_err(|_| AppError::Upstream("component provider prepare timed out".into()))?
        .map_err(|error| AppError::Upstream(format!("component provider task failed: {error}")))??
        .ok_or_else(|| {
            AppError::Upstream("component provider adapter is declared but unavailable".into())
        })
}

pub(super) async fn normalize_component_provider(
    state: &AppState,
    provider_id: &str,
    context: RequestContext,
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
) -> Result<NormalizedProviderResponse, AppError> {
    let plugins = state.plugins.clone();
    let provider_id = provider_id.to_owned();
    let permit = tokio::time::timeout(Duration::from_secs(1), PLUGIN_EXECUTION_PERMITS.acquire())
        .await
        .map_err(|_| AppError::Upstream("plugin execution capacity is exhausted".into()))?
        .map_err(|_| AppError::Internal)?;
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        plugins.normalize_provider_response(&provider_id, context, status, &headers, &body)
    });
    tokio::time::timeout(Duration::from_secs(35), task)
        .await
        .map_err(|_| AppError::Upstream("component provider normalize timed out".into()))?
        .map_err(|error| AppError::Upstream(format!("component provider task failed: {error}")))??
        .ok_or_else(|| {
            AppError::Upstream("component provider adapter is declared but unavailable".into())
        })
}

pub(super) fn component_provider_url(base_url: &str, path: &str) -> Result<String, AppError> {
    let base = network::checked_http_url(base_url)?;
    let target = network::checked_http_url(&format!("{}{}", base_url.trim_end_matches('/'), path))?;
    if target.origin() != base.origin() {
        return Err(AppError::Upstream(
            "component provider path changed the configured upstream origin".into(),
        ));
    }
    Ok(target.into())
}

pub(super) fn component_provider_timeout(config: &Value) -> Duration {
    Duration::from_secs(
        config
            .get("timeout_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(120)
            .clamp(1, 120),
    )
}
