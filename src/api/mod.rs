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

mod archive_quarantine;
mod auth;
mod billing;
mod cloud_entitlements;
mod control_requests;
mod credentials;
mod generation;
mod health;
mod limits;
mod model_routes;
mod plugins;
mod proxy;
mod request_detail;
mod router;
mod routes;
mod self_service;
mod traffic;
mod upstreams;
mod usage_analysis;
mod web;

use archive_quarantine::{
    get_archive_quarantine, list_archive_quarantine, resolve_archive_quarantine,
};
use auth::{
    authenticate_control_before_body, authenticate_downstream, authenticate_gateway_before_body,
    management_tenant, require_global_service, require_service, require_service_any,
    require_service_tenant,
};
use billing::*;
use cloud_entitlements::sync_memeloop_cloud_subscription;
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
use limits::*;
use model_routes::{
    create_model_route, delete_model_route, list_model_routes, set_model_route_enabled,
    update_model_route,
};
use plugins::{get_plugin_configuration, put_plugin_configuration};
use request_detail::*;
pub use router::{router, router_for_role};
use routes::{control_router, gateway_router};
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
        CloudCredentialEntitlementBinding, CloudSubscriptionEventInput, CreateGenerationJobResult,
        CreateKeyInput, CreateServiceTokenInput, CreateUpstreamAccountInput, EntitlementOperation,
        FinishProxyRequest, FinishProxyRequestResult, FinishSynchronousImageRequest,
        FinishSynchronousImageResult, GenerationJobIdempotency, ProxyConversationInput,
        ReauthorizeUpstreamAccountInput, ReconcileEntitlementInput, ReplaceEntitlementInput,
        RequestListFilter, StartGenerationJobInput, StartProxyRequest,
        StartSynchronousImageRequest, StartSynchronousImageResult, StatsFilter,
        SynchronousImageIdempotencyClaim, UpdateUpstreamAccountInput, unix_millis,
    },
    error::AppError,
    model::{AuthenticatedKey, AuthenticatedService, KeyPolicy, TokenUsage},
    network::{self, OutboundScope},
    oauth::{
        CursorOAuthEndpoints, CursorPollResult, OAuthReauthorizationTarget, StartCursorLogin,
        StartSubscriptionBridgeLogin, SubscriptionBridgePollResult, poll_cursor_login,
        poll_subscription_bridge_login, refresh_cursor_credential,
        refresh_managed_oauth_credential, resolve_managed_oauth_refresh_adapter,
        start_cursor_login, start_subscription_bridge_login,
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

#[cfg(test)]
mod tests;
