use std::{str::FromStr, time::Instant};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_stream::wrappers::ReceiverStream;
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};
use uuid::Uuid;

use crate::{
    AppState, crypto,
    db::{CreateKeyInput, FinishRequest, NewRequest},
    error::AppError,
    model::{AuthenticatedKey, KeyPolicy},
};

const REQUEST_ID_HEADER: &str = "x-mtc-request-id";

pub fn router(state: AppState) -> Router {
    let request_id_header = header::HeaderName::from_static(REQUEST_ID_HEADER);
    Router::new()
        .route("/healthz", get(health))
        .route("/portal", get(portal))
        .route("/internal/v1/keys", post(create_key))
        .route("/internal/v1/keys/{key_id}/rotate", post(rotate_key))
        .route("/internal/v1/prices/{currency}/{model}", post(upsert_price))
        .route(
            "/internal/v1/accounts/{account_id}/grants",
            post(grant_balance),
        )
        .route("/self/v1/key", get(self_key))
        .route("/self/v1/requests", get(self_requests))
        .route("/self/v1/requests/{request_id}", get(self_request_detail))
        .route("/self/v1/stats", get(self_stats))
        .route("/self/v1/conversations", get(self_conversations))
        .route(
            "/self/v1/conversations/{cluster_id}",
            get(self_conversation_detail),
        )
        .route("/v1/responses", post(proxy_openai_responses))
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(proxy_openai_chat))
        .route("/v1/embeddings", post(proxy_openai_embeddings))
        .route("/v1/messages", post(proxy_anthropic))
        .route(
            "/v1/messages/count_tokens",
            post(proxy_anthropic_count_tokens),
        )
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .layer(PropagateRequestIdLayer::new(request_id_header.clone()))
        .layer(SetRequestIdLayer::new(request_id_header, MakeRequestUuid))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

async fn portal() -> Html<&'static str> {
    Html(include_str!("portal.html"))
}

#[derive(Debug, Deserialize)]
struct CreateKeyRequest {
    #[serde(default = "default_tenant")]
    tenant_external_id: String,
    principal_external_id: String,
    alias: String,
    #[serde(default = "default_currency")]
    currency: String,
    #[serde(default)]
    policy: KeyPolicy,
    #[serde(default = "zero_amount")]
    initial_balance: String,
}

fn default_tenant() -> String {
    "default".to_owned()
}

fn default_currency() -> String {
    "USD".to_owned()
}

fn zero_amount() -> String {
    "0".to_owned()
}

async fn create_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateKeyRequest>,
) -> Result<impl IntoResponse, AppError> {
    require_service(&headers, &state)?;
    let initial_balance = Decimal::from_str(&body.initial_balance)
        .map_err(|_| AppError::BadRequest("initial_balance must be a decimal string".into()))?;
    if initial_balance.is_sign_negative() {
        return Err(AppError::BadRequest(
            "initial_balance cannot be negative".into(),
        ));
    }
    let issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: body.tenant_external_id,
                principal_external_id: body.principal_external_id,
                alias: body.alias,
                currency: body.currency,
                policy: body.policy,
                initial_balance,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(issued)))
}

async fn rotate_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    require_service(&headers, &state)?;
    let issued = state
        .db
        .rotate_key(key_id, state.config.key_pepper.as_bytes())
        .await?;
    Ok(Json(issued))
}

#[derive(Debug, Deserialize)]
struct PriceRequest {
    input_per_million: String,
    output_per_million: String,
}

async fn upsert_price(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((currency, model)): Path<(String, String)>,
    Json(body): Json<PriceRequest>,
) -> Result<impl IntoResponse, AppError> {
    require_service(&headers, &state)?;
    let input = parse_decimal(&body.input_per_million, "input_per_million")?;
    let output = parse_decimal(&body.output_per_million, "output_per_million")?;
    let price = state
        .db
        .upsert_model_price(&model, &currency, input, output)
        .await?;
    Ok(Json(json!({
        "price_id": price.id,
        "model": model,
        "currency": currency.to_uppercase(),
        "input_per_million": body.input_per_million,
        "output_per_million": body.output_per_million
    })))
}

#[derive(Debug, Deserialize)]
struct GrantRequest {
    amount: String,
    source: String,
}

async fn grant_balance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Json(body): Json<GrantRequest>,
) -> Result<impl IntoResponse, AppError> {
    require_service(&headers, &state)?;
    let amount = parse_decimal(&body.amount, "amount")?;
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::BadRequest("Idempotency-Key is required".into()))?;
    let granted = state
        .db
        .grant(account_id, amount, &body.source, idempotency_key)
        .await?;
    Ok((StatusCode::CREATED, Json(json!({"granted": granted}))))
}

async fn self_key(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    Ok(Json(state.db.key_view(&key).await?))
}

#[derive(Debug, Deserialize)]
struct RequestsQuery {
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    50
}

async fn self_requests(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RequestsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    Ok(Json(state.db.list_requests(key.key_id, query.limit).await?))
}

async fn self_request_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(request_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    let refs = state
        .db
        .request_archive_refs(key.key_id, request_id)
        .await?;
    let (request_body, request_complete) = archive_value(&state, &refs.request_object).await;
    let (response_body, response_complete) = match refs.response_object.as_deref() {
        Some(location) => archive_value(&state, location).await,
        None => (Value::Null, false),
    };
    Ok(Json(crate::model::RequestDetail {
        view: refs.view,
        request_body,
        response_body,
        archive_complete: request_complete && response_complete,
    }))
}

async fn self_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    Ok(Json(state.db.stats(key.key_id).await?))
}

async fn self_conversations(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    Ok(Json(state.db.conversation_clusters(key.key_id).await?))
}

async fn self_conversation_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(cluster_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    Ok(Json(
        state
            .db
            .conversation_cluster_detail(key.key_id, cluster_id)
            .await?,
    ))
}

async fn list_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    let models = state.db.allowed_models(&key).await?;
    Ok(Json(json!({
        "object": "list",
        "data": models.into_iter().map(|id| json!({
            "id": id,
            "object": "model",
            "owned_by": "memeloop-token-center"
        })).collect::<Vec<_>>()
    })))
}

async fn proxy_openai_chat(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    proxy(state, headers, body, Protocol::OpenAiChat).await
}

async fn proxy_openai_responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    proxy(state, headers, body, Protocol::OpenAiResponses).await
}

async fn proxy_openai_embeddings(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    proxy(state, headers, body, Protocol::OpenAiEmbeddings).await
}

async fn proxy_anthropic(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    proxy(state, headers, body, Protocol::AnthropicMessages).await
}

async fn proxy_anthropic_count_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    proxy(state, headers, body, Protocol::AnthropicCountTokens).await
}

#[derive(Clone, Copy)]
enum Protocol {
    OpenAiChat,
    OpenAiResponses,
    OpenAiEmbeddings,
    AnthropicMessages,
    AnthropicCountTokens,
}

impl Protocol {
    fn name(self) -> &'static str {
        match self {
            Self::OpenAiChat | Self::OpenAiResponses | Self::OpenAiEmbeddings => "openai",
            Self::AnthropicMessages | Self::AnthropicCountTokens => "anthropic",
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::OpenAiChat => "/v1/chat/completions",
            Self::OpenAiResponses => "/v1/responses",
            Self::OpenAiEmbeddings => "/v1/embeddings",
            Self::AnthropicMessages => "/v1/messages",
            Self::AnthropicCountTokens => "/v1/messages/count_tokens",
        }
    }

    fn is_openai(self) -> bool {
        matches!(
            self,
            Self::OpenAiChat | Self::OpenAiResponses | Self::OpenAiEmbeddings
        )
    }
}

async fn proxy(
    state: AppState,
    headers: HeaderMap,
    body: Bytes,
    protocol: Protocol,
) -> Result<Response, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    let request_json: Value = serde_json::from_slice(&body)
        .map_err(|_| AppError::BadRequest("request body must be valid JSON".into()))?;
    let model = request_json
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("model is required".into()))?
        .to_owned();
    if !key.policy.allows_model(&model) {
        return Err(AppError::Forbidden);
    }
    let price = state.db.model_price(&model, &key.currency).await?;
    let input_token_ceiling = i64::try_from(body.len()).unwrap_or(i64::MAX);
    let default_output_token_ceiling = if matches!(
        protocol,
        Protocol::OpenAiEmbeddings | Protocol::AnthropicCountTokens
    ) {
        0
    } else {
        4_096
    };
    let output_token_ceiling = request_json
        .get("max_output_tokens")
        .or_else(|| request_json.get("max_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(default_output_token_ceiling)
        .max(0);
    let reservation = state
        .db
        .reserve_usage(&key, &price, input_token_ceiling, output_token_ceiling)
        .await?;

    let (base_url, upstream_key) = if protocol.is_openai() {
        (
            state.config.upstream_openai_url.as_ref(),
            state.config.upstream_openai_key.as_ref(),
        )
    } else {
        (
            state.config.upstream_anthropic_url.as_ref(),
            state.config.upstream_anthropic_key.as_ref(),
        )
    };
    let base_url = base_url.ok_or_else(|| AppError::Upstream(protocol.name().into()))?;
    let path = protocol.path();
    let request_id = Uuid::now_v7();
    let prefix = format!("tenants/{}/requests/{request_id}", key.tenant_id);
    let request_object = format!("{prefix}/request.json");
    let response_object = format!("{prefix}/response.bin");
    let stored_request = match state.archive.put(&request_object, body.clone()).await {
        Ok(()) => request_object,
        Err(error) => {
            tracing::warn!(%request_id, %error, "request archive gap");
            format!("gap://{request_id}/request")
        }
    };
    state
        .db
        .record_request_started(NewRequest {
            request_id,
            key_id: key.key_id,
            tenant_id: key.tenant_id,
            protocol: protocol.name().to_owned(),
            model,
            request_object: stored_request,
            reservation_id: reservation.id,
        })
        .await?;
    let conversation_hint = conversation_hint(&headers, &request_json);
    let client_name = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok());
    if let Err(error) = state
        .db
        .record_conversation_observation(
            &key,
            request_id,
            &request_json,
            conversation_hint.as_deref(),
            client_name,
        )
        .await
    {
        tracing::warn!(%request_id, %error, "logical conversation inference failed");
    }

    let started = Instant::now();
    let mut request = state
        .http
        .post(format!("{}{}", base_url.trim_end_matches('/'), path))
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::ACCEPT,
            headers
                .get(header::ACCEPT)
                .cloned()
                .unwrap_or(HeaderValue::from_static("application/json")),
        )
        .body(body);
    if let Some(upstream_key) = upstream_key {
        request = if protocol.is_openai() {
            request.bearer_auth(upstream_key)
        } else {
            request.header("x-api-key", upstream_key)
        };
    }
    if let Some(version) = headers.get("anthropic-version") {
        request = request.header("anthropic-version", version);
    }

    let upstream = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            let _ = state.db.settle_usage(&reservation, 0, 0).await;
            state
                .db
                .record_request_finished(FinishRequest {
                    request_id,
                    status_code: 502,
                    duration_ms: started.elapsed().as_millis() as i64,
                    input_tokens: 0,
                    output_tokens: 0,
                    cost_micros: 0,
                    error_code: Some("upstream_connection".to_owned()),
                    response_object: format!("gap://{request_id}/response"),
                })
                .await?;
            return Err(AppError::Upstream(error.to_string()));
        }
    };
    let status = upstream.status();
    let content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();
    let archive_writer = match state.archive.start_writer(&response_object).await {
        Ok(writer) => Some(writer),
        Err(error) => {
            tracing::warn!(%request_id, %error, "response archive gap");
            None
        }
    };
    let (body_sender, body_receiver) = tokio::sync::mpsc::channel(8);
    let background_state = state.clone();
    let status_code = i64::from(status.as_u16());
    tokio::spawn(async move {
        let mut upstream_stream = upstream.bytes_stream();
        let mut archive_writer = archive_writer;
        let mut usage_capture = Vec::new();
        let mut transport_error = None;
        while let Some(next) = upstream_stream.next().await {
            match next {
                Ok(chunk) => {
                    append_bounded(&mut usage_capture, &chunk, 2 * 1024 * 1024);
                    if let Some(mut writer) = archive_writer.take() {
                        match writer.write(chunk.clone()).await {
                            Ok(()) => archive_writer = Some(writer),
                            Err(error) => {
                                tracing::warn!(%request_id, %error, "response archive stream gap");
                                let _ = writer.abort().await;
                            }
                        }
                    }
                    let _ = body_sender.send(Ok::<Bytes, std::io::Error>(chunk)).await;
                }
                Err(error) => {
                    transport_error = Some(error.to_string());
                    let _ = body_sender
                        .send(Err(std::io::Error::other(error.to_string())))
                        .await;
                    break;
                }
            }
        }
        let archive_complete = if let Some(writer) = archive_writer {
            match writer.finish().await {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(%request_id, %error, "response archive finalize gap");
                    false
                }
            }
        } else {
            false
        };
        let (input_tokens, output_tokens) =
            extract_usage(&usage_capture).unwrap_or((input_token_ceiling, output_token_ceiling));
        let actual_cost_micros = background_state
            .db
            .settle_usage(&reservation, input_tokens, output_tokens)
            .await
            .unwrap_or(reservation.reserved_micros);
        let error_code = transport_error
            .as_ref()
            .map(|_| "upstream_stream".to_owned())
            .or_else(|| (!status.is_success()).then(|| format!("http_{}", status.as_u16())));
        let stored_response = if archive_complete {
            response_object
        } else {
            format!("gap://{request_id}/response")
        };
        if let Err(error) = background_state
            .db
            .record_request_finished(FinishRequest {
                request_id,
                status_code,
                duration_ms: started.elapsed().as_millis() as i64,
                input_tokens,
                output_tokens,
                cost_micros: actual_cost_micros,
                error_code,
                response_object: stored_response,
            })
            .await
        {
            tracing::error!(%request_id, %error, "failed to finalize request record");
        }
    });
    let mut response = Response::builder()
        .status(status)
        .header(REQUEST_ID_HEADER, request_id.to_string());
    if let Some(content_type) = content_type {
        response = response.header(header::CONTENT_TYPE, content_type);
    }
    response
        .body(Body::from_stream(ReceiverStream::new(body_receiver)))
        .map_err(|_| AppError::Internal)
}

fn extract_usage(body: &[u8]) -> Option<(i64, i64)> {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        return usage_from_value(&value);
    }
    let mut input: Option<i64> = None;
    let mut output: Option<i64> = None;
    for line in body.split(|byte| *byte == b'\n') {
        let line = line.strip_prefix(b"data: ").unwrap_or(line);
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if let Some((next_input, next_output)) = usage_from_value(&value) {
            input = Some(input.unwrap_or_default().max(next_input));
            output = Some(output.unwrap_or_default().max(next_output));
        }
    }
    input.zip(output)
}

fn usage_from_value(value: &Value) -> Option<(i64, i64)> {
    let usage = value
        .get("usage")
        .or_else(|| value.pointer("/message/usage"))
        .unwrap_or(&Value::Null);
    let input = usage
        .get("input_tokens")
        .or_else(|| usage.get("prompt_tokens"))
        .and_then(Value::as_i64);
    let output = usage
        .get("output_tokens")
        .or_else(|| usage.get("completion_tokens"))
        .and_then(Value::as_i64);
    match (input, output) {
        (Some(input), Some(output)) => Some((input, output)),
        (Some(input), None) => Some((input, 0)),
        (None, Some(output)) => Some((0, output)),
        (None, None) => None,
    }
}

fn append_bounded(capture: &mut Vec<u8>, chunk: &[u8], maximum: usize) {
    if chunk.len() >= maximum {
        capture.clear();
        capture.extend_from_slice(&chunk[chunk.len() - maximum..]);
        return;
    }
    let overflow = capture
        .len()
        .saturating_add(chunk.len())
        .saturating_sub(maximum);
    if overflow > 0 {
        capture.drain(..overflow);
    }
    capture.extend_from_slice(chunk);
}

fn conversation_hint(headers: &HeaderMap, body: &Value) -> Option<String> {
    for name in [
        "x-mtc-conversation-id",
        "x-claude-code-session-id",
        "x-codex-session-id",
        "x-conversation-id",
        "x-session-id",
    ] {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok())
            && !value.is_empty()
        {
            return Some(value.to_owned());
        }
    }
    for pointer in [
        "/metadata/conversation_id",
        "/metadata/session_id",
        "/metadata/thread_id",
        "/conversation_id",
        "/session_id",
        "/thread_id",
        "/prompt_cache_key",
    ] {
        if let Some(value) = body.pointer(pointer).and_then(Value::as_str)
            && !value.is_empty()
        {
            return Some(value.to_owned());
        }
    }
    None
}

async fn archive_value(state: &AppState, location: &str) -> (Value, bool) {
    if location.starts_with("gap://") {
        return (Value::Null, false);
    }
    match state.archive.get(location).await {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(value) => (value, true),
            Err(_) => (
                Value::String(String::from_utf8_lossy(&bytes).into_owned()),
                true,
            ),
        },
        Err(error) => {
            tracing::warn!(%location, %error, "archived request object is unavailable");
            (Value::Null, false)
        }
    }
}

fn parse_decimal(value: &str, field: &str) -> Result<Decimal, AppError> {
    Decimal::from_str(value)
        .map_err(|_| AppError::BadRequest(format!("{field} must be a decimal string")))
}

fn require_service(headers: &HeaderMap, state: &AppState) -> Result<(), AppError> {
    let provided = bearer(headers).ok_or(AppError::Unauthorized)?;
    if !crypto::constant_time_eq(provided.as_bytes(), state.config.service_token.as_bytes()) {
        return Err(AppError::Unauthorized);
    }
    Ok(())
}

async fn authenticate_downstream(
    headers: &HeaderMap,
    state: &AppState,
) -> Result<AuthenticatedKey, AppError> {
    let provided = bearer(headers).ok_or(AppError::Unauthorized)?;
    state
        .db
        .authenticate_key(provided, state.config.key_pepper.as_bytes())
        .await
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}
