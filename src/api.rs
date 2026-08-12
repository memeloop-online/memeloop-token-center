use std::{
    convert::Infallible,
    path::PathBuf,
    str::FromStr,
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{
        Html, IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post, put},
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
    AppState,
    config::RuntimeRole,
    crypto,
    db::{
        CreateKeyInput, CreateModelRouteInput, CreateServiceTokenInput, CreateUpstreamAccountInput,
        FinishRequest, NewRequest, unix_millis,
    },
    error::AppError,
    model::{AuthenticatedKey, AuthenticatedService, KeyPolicy},
    oauth::{
        CursorOAuthEndpoints, CursorPollResult, StartCursorLogin, poll_cursor_login,
        refresh_cursor_credential, start_cursor_login,
    },
    provider::UpstreamCredential,
};

const REQUEST_ID_HEADER: &str = "x-mtc-request-id";

pub fn router(state: AppState) -> Router {
    router_for_role(state, RuntimeRole::All)
}

pub fn router_for_role(state: AppState, role: RuntimeRole) -> Router {
    let request_id_header = header::HeaderName::from_static(REQUEST_ID_HEADER);
    let mut application = Router::new().route("/healthz", get(health));
    application = application.route("/ui-assets/{*path}", get(web_asset));
    if role.serves_control() {
        application = application.merge(control_router());
    }
    if role.serves_gateway() {
        application = application.merge(gateway_router());
    }
    application
        .layer(DefaultBodyLimit::max(16 * 1024 * 1024))
        .layer(PropagateRequestIdLayer::new(request_id_header.clone()))
        .layer(SetRequestIdLayer::new(request_id_header, MakeRequestUuid))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

fn control_router() -> Router<AppState> {
    Router::new()
        .route("/operator", get(operator_index))
        .route("/internal/v1/keys", post(create_key))
        .route("/internal/v1/keys/{key_id}/rotate", post(rotate_key))
        .route("/internal/v1/service-tokens", post(create_service_token))
        .route(
            "/internal/v1/service-tokens/{service_id}/rotate",
            post(rotate_service_token),
        )
        .route("/internal/v1/provider-types", get(provider_types))
        .route("/internal/v1/schemas", get(configuration_schemas))
        .route("/internal/v1/oauth/cursor/start", post(start_cursor_oauth))
        .route("/internal/v1/oauth/cursor/poll", post(poll_cursor_oauth))
        .route(
            "/internal/v1/upstreams",
            get(list_upstreams).post(create_upstream),
        )
        .route("/internal/v1/requests", get(internal_requests))
        .route("/internal/v1/request-events", get(internal_request_events))
        .route(
            "/internal/v1/upstreams/{account_id}/credential",
            put(rotate_upstream_credential),
        )
        .route(
            "/internal/v1/upstreams/{account_id}/oauth/refresh",
            post(refresh_upstream_oauth),
        )
        .route("/internal/v1/model-routes", post(create_model_route))
        .route("/internal/v1/prices/{currency}/{model}", post(upsert_price))
        .route(
            "/internal/v1/accounts/{account_id}/grants",
            post(grant_balance),
        )
}

fn gateway_router() -> Router<AppState> {
    Router::new()
        .route("/portal", get(portal_index))
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
}

async fn health() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

async fn operator_index() -> Response {
    web_index(false).await
}

async fn portal_index() -> Response {
    web_index(true).await
}

fn web_root() -> PathBuf {
    std::env::var_os("MTC_WEB_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/share/memeloop-token-center/web"))
}

async fn web_index(allow_fallback: bool) -> Response {
    match tokio::fs::read(web_root().join("index.html")).await {
        Ok(body) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response(),
        Err(error) if allow_fallback => {
            tracing::warn!(%error, "built web application is unavailable; serving fallback portal");
            Html(include_str!("portal.html")).into_response()
        }
        Err(error) => {
            tracing::error!(%error, "built operator web application is unavailable");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "operator assets are not installed",
            )
                .into_response()
        }
    }
}

async fn web_asset(Path(path): Path<String>) -> Response {
    if path
        .split('/')
        .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let content_type = mime_guess::from_path(&path).first_or_octet_stream();
    match tokio::fs::read(web_root().join(path)).await {
        Ok(body) => (
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_str(content_type.as_ref())
                    .unwrap_or(HeaderValue::from_static("application/octet-stream")),
            )],
            body,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
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
    let service = require_service(&headers, &state, "keys:write").await?;
    require_service_tenant(&service, &body.tenant_external_id)?;
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
    let service = require_service(&headers, &state, "keys:write").await?;
    if let Some(tenant) = service.tenant_external_id.as_deref() {
        state.db.require_key_tenant(key_id, tenant).await?;
    }
    let issued = state
        .db
        .rotate_key(key_id, state.config.key_pepper.as_bytes())
        .await?;
    Ok(Json(issued))
}

#[derive(Debug, Deserialize)]
struct CreateServiceTokenRequest {
    name: String,
    scopes: Vec<String>,
    tenant_external_id: Option<String>,
}

async fn create_service_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateServiceTokenRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "service_tokens:write").await?;
    require_global_service(&service)?;
    let issued = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: body.name,
                scopes: body.scopes,
                tenant_external_id: body.tenant_external_id,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(issued)))
}

async fn rotate_service_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(service_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "service_tokens:write").await?;
    require_global_service(&service)?;
    Ok(Json(
        state
            .db
            .rotate_service_token(service_id, state.config.key_pepper.as_bytes())
            .await?,
    ))
}

async fn provider_types(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    require_service(&headers, &state, "providers:read").await?;
    Ok(Json(state.providers.list().to_vec()))
}

async fn configuration_schemas(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    require_service(&headers, &state, "schemas:read").await?;
    fn schema(source: &str) -> Result<Value, AppError> {
        serde_json::from_str(source).map_err(|_| AppError::Internal)
    }
    Ok(Json(json!({
        "core_config": schema(include_str!("../schemas/core-config.schema.json"))?,
        "key_create": schema(include_str!("../schemas/key-create.schema.json"))?,
        "key_policy": schema(include_str!("../schemas/key-policy.schema.json"))?,
        "model_route": schema(include_str!("../schemas/model-route.schema.json"))?,
        "provider_account": schema(include_str!("../schemas/provider-account.schema.json"))?,
        "service_token": schema(include_str!("../schemas/service-token.schema.json"))?
    })))
}

#[derive(Debug, Deserialize)]
struct StartCursorOAuthRequest {
    #[serde(default = "default_tenant")]
    tenant_external_id: String,
    account_name: String,
    #[serde(default = "default_provider_driver")]
    provider_driver: String,
    provider_config: Value,
    #[serde(default)]
    endpoints: CursorOAuthEndpoints,
}

async fn start_cursor_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<StartCursorOAuthRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "oauth:write").await?;
    require_service_tenant(&service, &body.tenant_external_id)?;
    if !state.providers.contains(&body.provider_driver) {
        return Err(AppError::BadRequest(format!(
            "unknown provider driver: {}",
            body.provider_driver
        )));
    }
    Ok(Json(start_cursor_login(
        StartCursorLogin {
            tenant_external_id: body.tenant_external_id,
            account_name: body.account_name,
            provider_driver: body.provider_driver,
            provider_config: body.provider_config,
            endpoints: body.endpoints,
        },
        state.config.key_pepper.as_bytes(),
        unix_millis(),
    )?))
}

#[derive(Debug, Deserialize)]
struct PollCursorOAuthRequest {
    session_token: String,
}

async fn poll_cursor_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PollCursorOAuthRequest>,
) -> Result<Response, AppError> {
    let service = require_service(&headers, &state, "oauth:write").await?;
    match poll_cursor_login(
        &state.http,
        &body.session_token,
        state.config.key_pepper.as_bytes(),
        unix_millis(),
    )
    .await?
    {
        CursorPollResult::Pending {
            retry_after_seconds,
        } => Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "pending",
                "retry_after_seconds": retry_after_seconds
            })),
        )
            .into_response()),
        CursorPollResult::Ready(ready) => {
            let ready = *ready;
            require_service_tenant(&service, &ready.tenant_external_id)?;
            let account = state
                .db
                .create_upstream_account(
                    CreateUpstreamAccountInput {
                        tenant_external_id: ready.tenant_external_id,
                        name: ready.account_name,
                        driver: ready.provider_driver,
                        config: ready.provider_config,
                        credential: ready.credential,
                        oauth_session_id: Some(ready.session_id),
                    },
                    state.config.key_pepper.as_bytes(),
                )
                .await?;
            Ok((StatusCode::CREATED, Json(account)).into_response())
        }
    }
}

async fn refresh_upstream_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "oauth:write").await?;
    if let Some(tenant) = service.tenant_external_id.as_deref() {
        state.db.require_upstream_tenant(account_id, tenant).await?;
    }
    let (account, credential) = state
        .db
        .upstream_account_with_credential(account_id, state.config.key_pepper.as_bytes())
        .await?;
    if account.auth_kind != "oauth" {
        return Err(AppError::BadRequest(
            "upstream account does not use OAuth".into(),
        ));
    }
    let driver = account
        .config
        .pointer("/oauth/driver")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("upstream OAuth driver is missing".into()))?;
    let refresh_url = account
        .config
        .pointer("/oauth/refresh_url")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("upstream OAuth refresh URL is missing".into()))?;
    let refreshed = match driver {
        "cursor" => {
            refresh_cursor_credential(&state.http, refresh_url, &credential, unix_millis()).await?
        }
        _ => {
            return Err(AppError::BadRequest(format!(
                "unsupported OAuth driver: {driver}"
            )));
        }
    };
    Ok(Json(
        state
            .db
            .rotate_upstream_credential(account_id, refreshed, state.config.key_pepper.as_bytes())
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
struct CreateUpstreamRequest {
    #[serde(default = "default_tenant")]
    tenant_external_id: String,
    name: String,
    #[serde(default = "default_provider_driver")]
    driver: String,
    config: Value,
    credential: UpstreamCredential,
}

fn default_provider_driver() -> String {
    "http-json".to_owned()
}

async fn create_upstream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateUpstreamRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:write").await?;
    require_service_tenant(&service, &body.tenant_external_id)?;
    if !state.providers.contains(&body.driver) {
        return Err(AppError::BadRequest(format!(
            "unknown provider driver: {}",
            body.driver
        )));
    }
    body.credential.validate(unix_millis())?;
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: body.tenant_external_id,
                name: body.name,
                driver: body.driver,
                config: body.config,
                credential: body.credential,
                oauth_session_id: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(account)))
}

async fn list_upstreams(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:read").await?;
    let tenant = service.tenant_external_id.as_deref().unwrap_or("default");
    Ok(Json(state.db.list_upstream_accounts(tenant).await?))
}

async fn internal_requests(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RequestsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "requests:read").await?;
    let tenant = service.tenant_external_id.as_deref().unwrap_or("default");
    Ok(Json(state.db.list_all_requests(tenant, query.limit).await?))
}

#[derive(Debug, Deserialize)]
struct RequestEventsQuery {
    after_event_at: Option<i64>,
    after_event_id: Option<Uuid>,
}

async fn internal_request_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RequestEventsQuery>,
) -> Result<Response, AppError> {
    let service = require_service(&headers, &state, "requests:read").await?;
    let tenant = service
        .tenant_external_id
        .unwrap_or_else(|| "default".to_owned());
    let database = state.db.clone();
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(64);
    tokio::spawn(async move {
        let mut event_at = query
            .after_event_at
            .unwrap_or_else(|| unix_millis().saturating_sub(5_000));
        let mut event_id = query.after_event_id;
        loop {
            match database
                .request_events_after(&tenant, event_at, event_id, 500)
                .await
            {
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
                    tracing::warn!(%error, %tenant, "request event tail query failed");
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
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

#[derive(Debug, Deserialize)]
struct RotateUpstreamCredentialRequest {
    credential: UpstreamCredential,
}

async fn rotate_upstream_credential(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Json(body): Json<RotateUpstreamCredentialRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:write").await?;
    if let Some(tenant) = service.tenant_external_id.as_deref() {
        state.db.require_upstream_tenant(account_id, tenant).await?;
    }
    body.credential.validate(unix_millis())?;
    Ok(Json(
        state
            .db
            .rotate_upstream_credential(
                account_id,
                body.credential,
                state.config.key_pepper.as_bytes(),
            )
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
struct CreateModelRouteRequest {
    #[serde(default = "default_tenant")]
    tenant_external_id: String,
    public_model: String,
    upstream_account_id: Uuid,
    upstream_model: String,
    protocol: String,
    #[serde(default)]
    priority: i64,
}

async fn create_model_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateModelRouteRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "routes:write").await?;
    require_service_tenant(&service, &body.tenant_external_id)?;
    let route = state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: body.tenant_external_id,
            public_model: body.public_model,
            upstream_account_id: body.upstream_account_id,
            upstream_model: body.upstream_model,
            protocol: body.protocol,
            priority: body.priority,
        })
        .await?;
    Ok((StatusCode::CREATED, Json(route)))
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
    let service = require_service(&headers, &state, "prices:write").await?;
    require_global_service(&service)?;
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
    let service = require_service(&headers, &state, "credits:write").await?;
    if let Some(tenant) = service.tenant_external_id.as_deref() {
        state.db.require_account_tenant(account_id, tenant).await?;
    }
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
    let resolved_route = state
        .db
        .resolve_upstream(
            key.tenant_id,
            &model,
            protocol.name(),
            state.config.key_pepper.as_bytes(),
        )
        .await?;
    let (
        base_url,
        upstream_credential,
        legacy_upstream_key,
        upstream_model,
        upstream_account_id,
        model_route_id,
    ) = if let Some(route) = resolved_route {
        if route.driver != "http-json" {
            return Err(AppError::Upstream(format!(
                "provider driver {} is not loaded",
                route.driver
            )));
        }
        route.credential.validate(unix_millis())?;
        (
            route.base_url,
            Some(route.credential),
            None,
            route.upstream_model,
            Some(route.account_id),
            Some(route.route_id),
        )
    } else {
        let (base_url, upstream_key) = if protocol.is_openai() {
            (
                state.config.upstream_openai_url.clone(),
                state.config.upstream_openai_key.clone(),
            )
        } else {
            (
                state.config.upstream_anthropic_url.clone(),
                state.config.upstream_anthropic_key.clone(),
            )
        };
        (
            base_url.ok_or_else(|| AppError::Upstream(protocol.name().into()))?,
            None,
            upstream_key,
            model.clone(),
            None,
            None,
        )
    };
    let mut forwarded_json = request_json.clone();
    if let Some(value) = forwarded_json.get_mut("model") {
        *value = Value::String(upstream_model);
    }
    let forwarded_body = serde_json::to_vec(&forwarded_json).map_err(|_| AppError::Internal)?;
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
            model: model.clone(),
            request_object: stored_request,
            reservation_id: reservation.id,
            upstream_account_id,
            model_route_id,
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
        .body(forwarded_body);
    if let Some(credential) = upstream_credential.as_ref() {
        request = credential.apply(request, unix_millis())?;
    } else if let Some(upstream_key) = legacy_upstream_key.as_ref() {
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

async fn require_service(
    headers: &HeaderMap,
    state: &AppState,
    scope: &str,
) -> Result<AuthenticatedService, AppError> {
    let provided = bearer(headers).ok_or(AppError::Unauthorized)?;
    let service =
        if crypto::constant_time_eq(provided.as_bytes(), state.config.service_token.as_bytes()) {
            AuthenticatedService::bootstrap()
        } else {
            state
                .db
                .authenticate_service_token(provided, state.config.key_pepper.as_bytes())
                .await?
        };
    if !service.allows(scope) {
        return Err(AppError::Forbidden);
    }
    Ok(service)
}

fn require_service_tenant(
    service: &AuthenticatedService,
    tenant_external_id: &str,
) -> Result<(), AppError> {
    if service
        .tenant_external_id
        .as_deref()
        .is_some_and(|tenant| tenant != tenant_external_id)
    {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

fn require_global_service(service: &AuthenticatedService) -> Result<(), AppError> {
    if service.tenant_external_id.is_some() {
        return Err(AppError::Forbidden);
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

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode, header},
    };
    use tower::ServiceExt;

    use super::*;
    use crate::config::Config;

    async fn test_state() -> (AppState, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("roles.db").display()
        );
        let state = AppState::initialize(Config::for_test(database_url))
            .await
            .unwrap();
        (state, directory)
    }

    fn json_post(path: &str) -> Request<Body> {
        Request::post(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap()
    }

    #[tokio::test]
    async fn gateway_and_control_have_disjoint_route_surfaces() {
        let (state, _directory) = test_state().await;
        let gateway = router_for_role(state.clone(), RuntimeRole::Gateway);
        let control = router_for_role(state, RuntimeRole::Control);

        let gateway_internal = gateway
            .clone()
            .oneshot(json_post("/internal/v1/keys"))
            .await
            .unwrap();
        assert_eq!(gateway_internal.status(), StatusCode::NOT_FOUND);
        let control_internal = control
            .clone()
            .oneshot(
                Request::post("/internal/v1/keys")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"principal_external_id":"probe","alias":"probe"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(control_internal.status(), StatusCode::UNAUTHORIZED);

        let control_model = control
            .oneshot(json_post("/v1/chat/completions"))
            .await
            .unwrap();
        assert_eq!(control_model.status(), StatusCode::NOT_FOUND);
        let gateway_model = gateway
            .oneshot(json_post("/v1/chat/completions"))
            .await
            .unwrap();
        assert_eq!(gateway_model.status(), StatusCode::UNAUTHORIZED);
    }
}
