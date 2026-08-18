use std::{
    collections::BTreeMap,
    future::Future,
    path::{Component, PathBuf},
    str::FromStr,
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, MatchedPath, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, patch, post, put},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_stream::wrappers::ReceiverStream;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};
use uuid::Uuid;

mod auth;
mod billing;
mod control_requests;
mod credentials;
mod generation;
mod health;
mod model_routes;
mod proxy;
mod request_detail;
mod self_service;
mod traffic;
mod upstreams;
mod usage_analysis;
mod web;

use auth::{
    authenticate_control_before_body, authenticate_downstream, authenticate_gateway_before_body,
    management_tenant, require_global_service, require_service, require_service_tenant,
};
use billing::*;
use control_requests::{
    ManagementTenantQuery, configuration_schemas, internal_generation_asset,
    internal_request_asset, internal_request_detail, internal_request_events, internal_requests,
    internal_stats, list_tenants, plugin_manifests, provider_types,
};
use credentials::*;
use generation::*;
use health::{
    deprecated_health, liveness, observe_http, prometheus_metrics, readiness, security_headers,
    version,
};
use model_routes::{
    create_model_route, delete_model_route, list_model_routes, set_model_route_enabled,
    update_model_route,
};
use request_detail::*;
use self_service::*;
use traffic::{
    Protocol, apply_traffic_policy, component_provider_timeout, component_provider_url,
    inject_controlled_output_ceiling, normalize_component_provider, prepare_component_provider,
    proxy_anthropic, proxy_anthropic_count_tokens, proxy_openai_chat, proxy_openai_embeddings,
    proxy_openai_responses,
};
use upstreams::*;
use web::{operator_index, portal_index, web_asset};

pub(crate) use upstreams::refresh_managed_upstream_oauth;

use crate::{
    AppState,
    archive_staging::{ARCHIVE_STAGING_WRITE_HEARTBEAT_MILLIS, ArchiveStagingPurpose},
    config::RuntimeRole,
    db::{
        AttachGenerationJobResult, AttachSynchronousImageRequestObject, CancelEntitlementInput,
        CreateGenerationJobResult, CreateKeyInput, CreateServiceTokenInput,
        CreateUpstreamAccountInput, EntitlementOperation, FinishProxyRequest,
        FinishProxyRequestResult, FinishSynchronousImageRequest, FinishSynchronousImageResult,
        GenerationJobIdempotency, ProxyConversationInput, ReconcileEntitlementInput,
        ReplaceEntitlementInput, RequestListFilter, StartGenerationJobInput, StartProxyRequest,
        StartSynchronousImageRequest, StartSynchronousImageResult, StatsFilter,
        SynchronousImageIdempotencyClaim, UpdateUpstreamAccountInput, unix_millis,
    },
    error::AppError,
    model::{AuthenticatedKey, AuthenticatedService, KeyPolicy, TokenUsage},
    network::{self, OutboundScope},
    oauth::{
        CursorOAuthEndpoints, CursorPollResult, StartCursorLogin, StartSubscriptionBridgeLogin,
        SubscriptionBridgePollResult, poll_cursor_login, poll_subscription_bridge_login,
        refresh_cursor_credential, refresh_managed_oauth_credential,
        resolve_managed_oauth_refresh_adapter, start_cursor_login, start_subscription_bridge_login,
    },
    plugin::{PreparedProviderRequest, memeloop::token_center::types::RequestContext},
    provider::{UpstreamCredential, validate_config},
    proxy_lifecycle::{
        MAX_DOWNSTREAM_SEND_WAIT, MAX_PROXY_LIFETIME, MAX_UNCONFIRMED_DELIVERY_BYTES,
        abandon_proxy_archive_attempt, attach_proxy_archive_with_retry,
        begin_proxy_archive_attempt, confirm_proxy_delivery_with_retry,
        finish_proxy_request_with_retry, heartbeat_proxy_archive_attempt,
        prepare_proxy_delivery_with_retry, response_archive_requires_cleanup,
    },
};

#[cfg(test)]
use crate::db::CreateModelRouteInput;

const REQUEST_ID_HEADER: &str = "x-mtc-request-id";
const MAX_SUBSCRIPTION_BRIDGE_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_IMAGE_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_CPA_IMPORT_BODY: usize = 34 * 1024 * 1024;
const MAX_ARCHIVE_DETAIL_RESPONSE: usize = 4 * 1024 * 1024;
const MAX_PROXY_RESPONSE_BODY: usize = 64 * 1024 * 1024;
const MAX_DEFAULT_REQUEST_BODY: usize = 4 * 1024 * 1024;
const MAX_IMAGE_REQUEST_BODY: usize = 16 * 1024 * 1024;
const MAX_RESPONSES_SSE_EVENT_BYTES: usize = 256 * 1024;
const SYNCHRONOUS_IMAGE_DEADLINE: Duration = Duration::from_secs(12 * 60);
const GATEWAY_IN_FLIGHT_REQUESTS: usize = 16;
const MAX_REPORTED_TOKENS: i64 = 1_000_000_000;
static IMAGE_RESPONSE_PERMITS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

pub fn router(state: AppState) -> Router {
    router_for_role(state, RuntimeRole::All)
}

pub fn router_for_role(state: AppState, role: RuntimeRole) -> Router {
    let request_id_header = header::HeaderName::from_static(REQUEST_ID_HEADER);
    let mut application = Router::new()
        .route("/healthz", get(deprecated_health))
        .route("/livez", get(liveness))
        .route("/readyz", get(readiness));
    if matches!(role, RuntimeRole::Control | RuntimeRole::All) {
        application = application
            .route("/metrics", get(prometheus_metrics))
            .route("/version", get(version));
    }
    application = application.route("/ui-assets/{*path}", get(web_asset));
    if role.serves_control() {
        application = application.merge(control_router(state.clone()));
    }
    if role.serves_gateway() {
        application = application.merge(gateway_router(state.clone()));
    }
    application
        .layer(DefaultBodyLimit::max(MAX_DEFAULT_REQUEST_BODY))
        .layer(middleware::from_fn(security_headers))
        .layer(PropagateRequestIdLayer::new(request_id_header.clone()))
        .layer(SetRequestIdLayer::new(request_id_header, MakeRequestUuid))
        .layer(TraceLayer::new_for_http())
        .layer(middleware::from_fn_with_state(state.clone(), observe_http))
        .with_state(state)
}

fn control_router(state: AppState) -> Router<AppState> {
    let authenticated = Router::new()
        .route("/internal/v1/keys", get(list_keys).post(create_key))
        .route("/internal/v1/keys/{key_id}/rotate", post(rotate_key))
        .route("/internal/v1/keys/{key_id}/alias", patch(rename_key))
        .route("/internal/v1/keys/{key_id}/limits", get(key_limits))
        .route("/internal/v1/keys/{key_id}/policy", put(update_key_policy))
        .route("/internal/v1/keys/{key_id}/status", patch(set_key_status))
        .route(
            "/internal/v1/keys/{key_id}/legacy-credentials",
            post(register_legacy_key_credential),
        )
        .route(
            "/internal/v1/service-tokens",
            get(list_service_tokens).post(create_service_token),
        )
        .route(
            "/internal/v1/service-tokens/{service_id}/rotate",
            post(rotate_service_token),
        )
        .route(
            "/internal/v1/service-tokens/{service_id}/status",
            patch(set_service_token_status),
        )
        .route("/internal/v1/provider-types", get(provider_types))
        .route("/internal/v1/tenants", get(list_tenants))
        .route("/internal/v1/plugins", get(plugin_manifests))
        .route("/internal/v1/schemas", get(configuration_schemas))
        .route("/internal/v1/oauth/cursor/start", post(start_cursor_oauth))
        .route("/internal/v1/oauth/cursor/poll", post(poll_cursor_oauth))
        .route(
            "/internal/v1/oauth/provider-adapter/start",
            post(start_provider_adapter_oauth),
        )
        .route(
            "/internal/v1/oauth/provider-adapter/poll",
            post(poll_cursor_oauth),
        )
        .route(
            "/internal/v1/oauth/subscription-bridge/start",
            post(start_subscription_bridge_oauth),
        )
        .route(
            "/internal/v1/oauth/subscription-bridge/poll",
            post(poll_subscription_bridge_oauth),
        )
        .route(
            "/internal/v1/upstreams",
            get(list_upstreams).post(create_upstream),
        )
        .route(
            "/internal/v1/imports/cpa/managed-oauth/capabilities",
            get(cpa_managed_oauth_capabilities),
        )
        .route(
            "/internal/v1/imports/cpa/managed-oauth",
            post(import_cpa_managed_oauth)
                .layer(DefaultBodyLimit::max(MAX_MANAGED_OAUTH_IMPORT_REQUEST)),
        )
        .route(
            "/internal/v1/imports/cpa/subscription-accounts",
            post(import_cpa_subscription_accounts)
                .layer(DefaultBodyLimit::max(MAX_CPA_IMPORT_BODY)),
        )
        .route("/internal/v1/requests", get(internal_requests))
        .route(
            "/internal/v1/requests/{request_id}",
            get(internal_request_detail),
        )
        .route(
            "/internal/v1/requests/{request_id}/assets/{asset_id}",
            get(internal_request_asset),
        )
        .route(
            "/internal/v1/generations/{job_id}/assets/{asset_id}",
            get(internal_generation_asset),
        )
        .route("/internal/v1/stats", get(internal_stats))
        .route(
            "/internal/v1/usage-analysis",
            get(usage_analysis::internal_usage_analysis),
        )
        .route("/internal/v1/request-events", get(internal_request_events))
        .route(
            "/internal/v1/upstreams/{account_id}/credential",
            put(rotate_upstream_credential),
        )
        .route(
            "/internal/v1/upstreams/{account_id}",
            put(update_upstream)
                .patch(set_upstream_status)
                .delete(delete_upstream),
        )
        .route(
            "/internal/v1/upstreams/{account_id}/health",
            post(probe_upstream_health),
        )
        .route(
            "/internal/v1/upstreams/{account_id}/oauth/refresh",
            post(refresh_upstream_oauth),
        )
        .route(
            "/internal/v1/model-routes",
            get(list_model_routes).post(create_model_route),
        )
        .route(
            "/internal/v1/model-routes/{route_id}",
            put(update_model_route)
                .patch(set_model_route_enabled)
                .delete(delete_model_route),
        )
        .route("/internal/v1/model-prices", get(list_model_prices))
        .route(
            "/internal/v1/model-prices/usage-summary",
            get(model_price_usage_summary),
        )
        .route("/internal/v1/model-prices/sync", post(sync_model_prices))
        .route("/internal/v1/prices/{currency}/{model}", post(upsert_price))
        .route(
            "/internal/v1/generation-prices/{currency}/{model}",
            post(upsert_generation_price),
        )
        .route(
            "/internal/v1/generation-prices",
            get(list_generation_prices),
        )
        .route(
            "/internal/v1/accounts/{account_id}/grants",
            post(grant_balance),
        )
        .route(
            "/internal/v1/accounts/{account_id}/grant-reversals",
            post(reverse_grant_balance),
        )
        .route(
            "/internal/v1/accounts/{account_id}/ledger",
            get(list_account_ledger),
        )
        .route(
            "/internal/v1/entitlements",
            get(list_entitlements).put(reconcile_entitlement),
        )
        .route("/internal/v1/entitlements/cancel", post(cancel_entitlement))
        .route(
            "/internal/v1/entitlements/replace",
            post(replace_entitlement),
        )
        .route_layer(middleware::from_fn_with_state(
            state,
            authenticate_control_before_body,
        ));
    Router::new()
        .route("/operator", get(operator_index))
        .merge(authenticated)
}

fn gateway_router(state: AppState) -> Router<AppState> {
    let authenticated = Router::new()
        .route("/self/v1/key", get(self_key))
        .route("/self/v1/key/limits", get(self_key_limits))
        .route("/self/v1/requests", get(self_requests))
        .route("/self/v1/requests/{request_id}", get(self_request_detail))
        .route(
            "/self/v1/requests/{request_id}/assets/{asset_id}",
            get(self_request_asset),
        )
        .route("/self/v1/stats", get(self_stats))
        .route("/self/v1/generations", get(self_generations))
        .route(
            "/self/v1/generations/{job_id}",
            get(self_generation).delete(cancel_self_generation),
        )
        .route(
            "/self/v1/generations/{job_id}/assets/{asset_id}",
            get(self_generation_asset),
        )
        .route("/self/v1/conversations", get(self_conversations))
        .route(
            "/self/v1/conversations/{cluster_id}",
            get(self_conversation_detail),
        )
        .route("/v1/responses", post(proxy_openai_responses))
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(proxy_openai_chat))
        .route("/v1/embeddings", post(proxy_openai_embeddings))
        .route("/v1/generations", post(create_generation))
        .route("/v1/videos/generations", post(create_generation))
        .route(
            "/v1/images/generations",
            post(create_image_generation).layer(DefaultBodyLimit::max(MAX_IMAGE_REQUEST_BODY)),
        )
        .route("/v1/messages", post(proxy_anthropic))
        .route(
            "/v1/messages/count_tokens",
            post(proxy_anthropic_count_tokens),
        )
        .route_layer(middleware::from_fn_with_state(
            state,
            authenticate_gateway_before_body,
        ))
        .layer(ConcurrencyLimitLayer::new(GATEWAY_IN_FLIGHT_REQUESTS));
    Router::new()
        .route("/portal", get(portal_index))
        .merge(authenticated)
}

#[cfg(test)]
mod tests;
