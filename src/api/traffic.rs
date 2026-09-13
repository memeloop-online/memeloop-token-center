use std::{collections::BTreeMap, time::Duration};

use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
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

#[cfg(test)]
tokio::task_local! {
    static TEST_COMPONENT_PREPARE_COUNTER: std::sync::Arc<std::sync::atomic::AtomicUsize>;
}

#[cfg(test)]
pub(super) async fn with_test_component_prepare_counter<F>(
    counter: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    future: F,
) -> F::Output
where
    F: std::future::Future,
{
    TEST_COMPONENT_PREPARE_COUNTER.scope(counter, future).await
}

pub(super) async fn proxy_openai_chat(
    State(state): State<AppState>,
    axum::Extension(memory): axum::Extension<
        std::sync::Arc<crate::gateway_body::memory::ProxyMemoryReservation>,
    >,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    super::proxy::proxy(state, headers, body, Protocol::OpenAiChat, memory).await
}

pub(super) async fn proxy_openai_responses(
    State(state): State<AppState>,
    axum::Extension(memory): axum::Extension<
        std::sync::Arc<crate::gateway_body::memory::ProxyMemoryReservation>,
    >,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    super::proxy::proxy(state, headers, body, Protocol::OpenAiResponses, memory).await
}

/// Temporary transport negotiation for clients that probe the Responses
/// WebSocket endpoint before falling back to the supported HTTP POST flow.
/// Keep this separate from the POST handler so native WebSocket support can
/// replace only this GET handler without changing authentication or routing.
pub(super) async fn negotiate_openai_responses_websocket() -> Response {
    let mut response = (
        StatusCode::UPGRADE_REQUIRED,
        Json(serde_json::json!({
            "error": {
                "code": "websocket_upgrade_required",
                "message": "Responses WebSocket transport is not available in this release; retry with HTTP POST"
            }
        })),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    response
}

pub(super) async fn proxy_openai_embeddings(
    State(state): State<AppState>,
    axum::Extension(memory): axum::Extension<
        std::sync::Arc<crate::gateway_body::memory::ProxyMemoryReservation>,
    >,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    super::proxy::proxy(state, headers, body, Protocol::OpenAiEmbeddings, memory).await
}

pub(super) async fn proxy_anthropic(
    State(state): State<AppState>,
    axum::Extension(memory): axum::Extension<
        std::sync::Arc<crate::gateway_body::memory::ProxyMemoryReservation>,
    >,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    super::proxy::proxy(state, headers, body, Protocol::AnthropicMessages, memory).await
}

pub(super) async fn proxy_anthropic_count_tokens(
    State(state): State<AppState>,
    axum::Extension(memory): axum::Extension<
        std::sync::Arc<crate::gateway_body::memory::ProxyMemoryReservation>,
    >,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    super::proxy::proxy(state, headers, body, Protocol::AnthropicCountTokens, memory).await
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
    pub(super) requested_model: String,
    pub(super) model: String,
    pub(super) upstream_account_hint: Option<Uuid>,
}

#[derive(Clone, Copy)]
pub(super) struct TrafficPolicyProtocols<'a> {
    /// The client-facing protocol exposed to traffic-policy plugins.
    pub(super) client: &'a str,
    /// The model-route protocol used for both pre- and post-rewrite grants.
    pub(super) routing: &'a str,
}

impl<'a> TrafficPolicyProtocols<'a> {
    pub(super) const fn same(protocol: &'a str) -> Self {
        Self {
            client: protocol,
            routing: protocol,
        }
    }
}

/// Traffic plugins run only after core key authentication. Both the client
/// model and the effective rewritten model must resolve through normalized
/// exact-route or route-group grants. A plugin may prefer one account within
/// the resulting authorized candidates, but cannot create a permission or
/// bypass exclusions.
pub(super) async fn apply_traffic_policy(
    state: &AppState,
    key: &AuthenticatedKey,
    protocols: TrafficPolicyProtocols<'_>,
    original_request_json: Value,
) -> Result<AppliedTraffic, AppError> {
    apply_traffic_policy_inner(state, key, protocols, original_request_json, None).await
}

pub(super) async fn apply_traffic_policy_with_memory(
    state: &AppState,
    key: &AuthenticatedKey,
    protocols: TrafficPolicyProtocols<'_>,
    original_request_json: Value,
    memory: std::sync::Arc<crate::gateway_body::memory::ProxyMemoryReservation>,
) -> Result<AppliedTraffic, AppError> {
    apply_traffic_policy_inner(state, key, protocols, original_request_json, Some(memory)).await
}

async fn apply_traffic_policy_inner(
    state: &AppState,
    key: &AuthenticatedKey,
    protocols: TrafficPolicyProtocols<'_>,
    original_request_json: Value,
    memory: Option<std::sync::Arc<crate::gateway_body::memory::ProxyMemoryReservation>>,
) -> Result<AppliedTraffic, AppError> {
    let requested_model = requested_traffic_model(&original_request_json)?;
    authorize_traffic_model(state, key, protocols.routing, &requested_model).await?;
    let applied = apply_traffic_plugin(
        state,
        key,
        protocols.client,
        original_request_json,
        requested_model,
        memory,
    )
    .await?;
    authorize_traffic_model(state, key, protocols.routing, &applied.model).await?;
    Ok(applied)
}

/// Replays must reproduce the same plugin rewrite before comparing the
/// effective request hash. This entry point is only used after the database
/// has confirmed that this key already owns the supplied Idempotency-Key; it
/// deliberately performs no route authorization itself.
pub(super) async fn apply_traffic_plugin_for_existing_idempotency(
    state: &AppState,
    key: &AuthenticatedKey,
    client_protocol: &str,
    original_request_json: Value,
) -> Result<AppliedTraffic, AppError> {
    let requested_model = requested_traffic_model(&original_request_json)?;
    apply_traffic_plugin(
        state,
        key,
        client_protocol,
        original_request_json,
        requested_model,
        None,
    )
    .await
}

pub(super) async fn authorize_applied_traffic_policy(
    state: &AppState,
    key: &AuthenticatedKey,
    routing_protocol: &str,
    applied: &AppliedTraffic,
) -> Result<(), AppError> {
    authorize_traffic_model(state, key, routing_protocol, &applied.requested_model).await?;
    if applied.model != applied.requested_model {
        authorize_traffic_model(state, key, routing_protocol, &applied.model).await?;
    }
    Ok(())
}

async fn authorize_traffic_model(
    state: &AppState,
    key: &AuthenticatedKey,
    routing_protocol: &str,
    model: &str,
) -> Result<(), AppError> {
    if state
        .db
        .credential_has_authorized_route(key.key_id, key.tenant_id, model, routing_protocol)
        .await?
    {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

fn requested_traffic_model(request_json: &Value) -> Result<String, AppError> {
    if !request_json.is_object() {
        return Err(AppError::BadRequest(
            "request body must be a JSON object".into(),
        ));
    }
    request_json
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty() && value.len() <= 200)
        .map(str::to_owned)
        .ok_or_else(|| AppError::BadRequest("model is required".into()))
}

async fn apply_traffic_plugin(
    state: &AppState,
    key: &AuthenticatedKey,
    client_protocol: &str,
    original_request_json: Value,
    requested_model: String,
    memory: Option<std::sync::Arc<crate::gateway_body::memory::ProxyMemoryReservation>>,
) -> Result<AppliedTraffic, AppError> {
    let plugins = state.plugins.clone();
    if !plugins.has_traffic_hooks() {
        return Ok(AppliedTraffic {
            request_json: original_request_json,
            requested_model: requested_model.clone(),
            model: requested_model,
            upstream_account_hint: None,
        });
    }
    let temporary_memory =
        if memory.is_some() {
            let input_length =
                crate::gateway_body::memory::json_encoded_length(&original_request_json)?;
            Some(state.proxy_memory_budget.temporary(
                64 * 1024 * 1024 + input_length.max(16 * 1024 * 1024).saturating_mul(5),
            )?)
        } else {
            None
        };
    let plugin_configurations = plugins
        .resolved_traffic_configurations(key.tenant_id)
        .await?;
    let plugin_request = original_request_json.clone();
    let plugin_context = RequestContext {
        tenant_id: key.tenant_id.to_string(),
        principal_id: key.principal_id.to_string(),
        key_id: key.key_id.to_string(),
        protocol: client_protocol.to_owned(),
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
        let _temporary_memory = temporary_memory;
        plugins.apply_traffic_with_config_and_memory(
            plugin_context,
            &plugin_request,
            &plugin_configurations,
            memory.as_deref(),
        )
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
        .unwrap_or_else(|| requested_model.clone());
    if model.trim().is_empty() || model.len() > 200 {
        return Err(AppError::Upstream(
            "plugin returned an invalid model".into(),
        ));
    }
    request_json["model"] = Value::String(model.clone());
    if crate::gateway_body::memory::json_encoded_length(&request_json)? > MAX_IMAGE_REQUEST_BODY {
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
        requested_model,
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
    memory: std::sync::Arc<crate::gateway_body::memory::ProxyMemoryReservation>,
) -> Result<PreparedProviderRequest, AppError> {
    #[cfg(test)]
    let _ = TEST_COMPONENT_PREPARE_COUNTER.try_with(|counter| {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    let plugins = state.plugins.clone();
    let provider_id = provider_id.to_owned();
    let permit = tokio::time::timeout(Duration::from_secs(1), PLUGIN_EXECUTION_PERMITS.acquire())
        .await
        .map_err(|_| AppError::Upstream("plugin execution capacity is exhausted".into()))?
        .map_err(|_| AppError::Internal)?;
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _memory = memory;
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
    let temporary_memory = state
        .proxy_memory_budget
        .temporary(64 * 1024 * 1024 + body.len().saturating_mul(6))?;
    let plugins = state.plugins.clone();
    let provider_id = provider_id.to_owned();
    let permit = tokio::time::timeout(Duration::from_secs(1), PLUGIN_EXECUTION_PERMITS.acquire())
        .await
        .map_err(|_| AppError::Upstream("plugin execution capacity is exhausted".into()))?
        .map_err(|_| AppError::Internal)?;
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _temporary_memory = temporary_memory;
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
