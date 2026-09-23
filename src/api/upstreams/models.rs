use std::{collections::BTreeMap, time::Duration};

use futures_util::StreamExt;
use serde::Serialize;

use super::super::*;
use crate::db::{
    DiscoveredUpstreamModel, ManagedModelRouteSyncResult, ReplaceModelCatalogResult,
    UpstreamModelCatalogView,
};

const MODEL_CATALOG_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_MODEL_CATALOG_BODY: usize = 2 * 1024 * 1024;
const MAX_MODEL_COUNT: usize = 10_000;
const MAX_MODEL_ID_BYTES: usize = 500;

#[derive(Debug, Serialize)]
struct CatalogSyncResult {
    #[serde(flatten)]
    catalog: UpstreamModelCatalogView,
    price_sync: CatalogPriceSyncResult,
}

#[derive(Debug, Serialize)]
struct ManagedCatalogSyncResult {
    catalog: UpstreamModelCatalogView,
    routes: ManagedModelRouteSyncResult,
    price_sync: CatalogPriceSyncResult,
}

#[derive(Debug, Serialize)]
struct CatalogPriceSyncResult {
    status: &'static str,
    currency: &'static str,
    imported: usize,
    preserved: usize,
    unmatched: usize,
    ambiguous: usize,
    failed_sources: Vec<String>,
    error_code: Option<&'static str>,
}

impl CatalogPriceSyncResult {
    fn skipped() -> Self {
        Self {
            status: "skipped",
            currency: "USD",
            imported: 0,
            preserved: 0,
            unmatched: 0,
            ambiguous: 0,
            failed_sources: Vec::new(),
            error_code: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CatalogBudget {
    total: Duration,
    read: Duration,
}

fn codex_catalog_budget(config: &Value) -> Result<CatalogBudget, &'static str> {
    let value = config.get("transport_policy");
    let policy = crate::provider::CodexTransportPolicy::parse(value)?;
    // Retry-only account policies must not silently opt a directory read into
    // the generation default (21 minutes). Explicit timeout fields do apply.
    let total = if value.is_some_and(|p| p.get("request_timeout_millis").is_some()) {
        Duration::from_millis(policy.request_timeout_millis)
    } else {
        MODEL_CATALOG_TIMEOUT
    };
    let read = if value.is_some_and(|p| p.get("read_timeout_millis").is_some()) {
        Duration::from_millis(policy.read_timeout_millis).min(total)
    } else {
        total
    };
    Ok(CatalogBudget { total, read })
}

#[derive(Debug, PartialEq, Eq)]
struct CatalogFailure {
    code: &'static str,
    stage: &'static str,
    kind: &'static str,
}

impl CatalogFailure {
    fn transport(stage: &'static str, timeout: bool, connect: bool) -> Self {
        Self {
            code: "connection_failed",
            stage,
            kind: if timeout {
                "timeout"
            } else if connect {
                "connect"
            } else if stage == "body" {
                "body"
            } else {
                "transport"
            },
        }
    }

    fn response(code: &'static str) -> Self {
        Self {
            code,
            stage: "response",
            kind: code,
        }
    }
}

fn log_catalog_failure(
    account: &crate::provider::UpstreamAccountView,
    failure: &CatalogFailure,
    started: std::time::Instant,
) {
    // Never log the underlying error: its URL or proxy source can carry secrets.
    tracing::warn!(account_id = %account.id, credential_generation = account.credential_generation,
        stage = failure.stage, failure_kind = failure.kind, error_code = failure.code,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "upstream model catalog request failed");
}

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
    let state = state.pin_application_plugins().await?;
    let tenant = account_tenant(&state, &service, account_id, query.tenant_external_id).await?;
    Ok(Json(
        sync_account_models(&state, account_id, &tenant, None).await?,
    ))
}

/// Explicit opt-in to route ownership; normal/background catalog refresh keeps
/// its historical directory-only route lifecycle semantics.
pub(in crate::api) async fn sync_upstream_models_and_routes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Query(query): Query<SyncUpstreamModelsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:write").await?;
    if !service.allows("routes:write") {
        return Err(AppError::Forbidden);
    }
    let tenant = account_tenant(&state, &service, account_id, query.tenant_external_id).await?;
    let state = state.pin_application_plugins().await?;
    let (account, credential) = state
        .db
        .upstream_account_with_credential(account_id, state.config.key_pepper.as_bytes())
        .await?;
    if account.tenant_external_id.as_deref() != Some(tenant.as_str()) {
        return Err(AppError::NotFound);
    }
    let generation = account.credential_generation;
    let timeout = if account.driver == "openai-codex" {
        codex_catalog_budget(&account.config)
            .map_err(|_| AppError::BadRequest("invalid Codex transport policy".into()))?
            .total
    } else {
        MODEL_CATALOG_TIMEOUT
    };
    let lease = Uuid::now_v7();
    let claimed = state
        .db
        .claim_upstream_model_catalog_sync_with_timeout(
            account_id,
            &tenant,
            generation,
            lease,
            timeout.as_millis() as u64,
        )
        .await?;
    let mut price_sync = CatalogPriceSyncResult::skipped();
    let routes = if !claimed {
        ManagedModelRouteSyncResult::skipped("sync_in_progress")
    } else {
        let discovery = tokio::time::timeout(
            timeout,
            discover_models(&state, &account, &credential, None, true),
        )
        .await
        .unwrap_or(Err("connection_failed"))
        .and_then(|snapshot| {
            if snapshot.1.is_empty() {
                Err("empty_catalog_protected")
            } else {
                Ok(snapshot)
            }
        });
        match discovery {
            Ok((source, models)) => {
                let replaced = state
                    .db
                    .replace_upstream_model_catalog(
                        account_id, &tenant, generation, lease, source, &models,
                    )
                    .await?;
                if replaced == ReplaceModelCatalogResult::Replaced {
                    let routes = state
                        .db
                        .reconcile_managed_model_routes(account_id, &tenant, generation, &models)
                        .await?;
                    if managed_route_snapshot_is_current(&routes) {
                        price_sync =
                            sync_discovered_model_prices(&state, account_id, &models).await;
                    }
                    routes
                } else {
                    ManagedModelRouteSyncResult::skipped("account_or_lease_changed")
                }
            }
            Err(code) => {
                // Preserve the legacy catalog failure vocabulary; the explicit
                // reconcile response carries the more specific protection code.
                let catalog_code = match code {
                    "partial_catalog"
                    | "empty_catalog_protected"
                    | "complete_catalog_unsupported" => "invalid_response",
                    code => code,
                };
                state
                    .db
                    .record_upstream_model_catalog_failure(
                        account_id,
                        &tenant,
                        generation,
                        lease,
                        catalog_code,
                    )
                    .await?;
                ManagedModelRouteSyncResult::skipped(code)
            }
        }
    };
    Ok(Json(ManagedCatalogSyncResult {
        catalog: state
            .db
            .upstream_model_catalog(account_id, &tenant, None, MAX_MODEL_COUNT as i64)
            .await?,
        routes,
        price_sync,
    }))
}

fn managed_route_snapshot_is_current(result: &ManagedModelRouteSyncResult) -> bool {
    !result.warnings.iter().any(|warning| {
        matches!(
            warning.as_str(),
            "account_changed" | "catalog_not_ready" | "catalog_changed"
        )
    })
}

pub(crate) fn trigger_upstream_model_sync(state: AppState, account_id: Uuid) {
    tokio::spawn(async move {
        sync_upstream_models_after_refresh(&state, account_id, None).await;
    });
}

pub(super) async fn sync_upstream_models_after_refresh(
    state: &AppState,
    account_id: Uuid,
    blocking: Option<&crate::worker::BlockingTasks>,
) {
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
    let _ = sync_account_models(state, account_id, tenant, blocking).await;
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
    blocking: Option<&crate::worker::BlockingTasks>,
) -> Result<CatalogSyncResult, AppError> {
    let pinned = state.clone().pin_application_plugins().await?;
    let state = &pinned;
    let (account, credential) = state
        .db
        .upstream_account_with_credential(account_id, state.config.key_pepper.as_bytes())
        .await?;
    if account.tenant_external_id.as_deref() != Some(tenant_external_id) {
        return Err(AppError::NotFound);
    }
    let generation = account.credential_generation;
    let catalog_timeout = if account.driver == "openai-codex" {
        codex_catalog_budget(&account.config)
            .map_err(|_| AppError::BadRequest("invalid Codex transport policy".into()))?
            .total
    } else {
        MODEL_CATALOG_TIMEOUT
    };
    let lease_id = Uuid::now_v7();
    if !state
        .db
        .claim_upstream_model_catalog_sync_with_timeout(
            account_id,
            tenant_external_id,
            generation,
            lease_id,
            catalog_timeout.as_millis() as u64,
        )
        .await?
    {
        return Ok(CatalogSyncResult {
            catalog: state
                .db
                .upstream_model_catalog(
                    account_id,
                    tenant_external_id,
                    None,
                    MAX_MODEL_COUNT as i64,
                )
                .await?,
            price_sync: CatalogPriceSyncResult::skipped(),
        });
    }
    let discovery = discover_models(state, &account, &credential, blocking, false).await;
    let mut price_sync = CatalogPriceSyncResult::skipped();
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
            // Routing availability commits first. Price-source outages must not
            // undo a confirmed disappearance or turn discovery into a failure.
            // Both the explicit endpoint and every background refresh enter here.
            price_sync = sync_discovered_model_prices(state, account_id, &models).await;
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
    Ok(CatalogSyncResult {
        catalog: state
            .db
            .upstream_model_catalog(account_id, tenant_external_id, None, MAX_MODEL_COUNT as i64)
            .await?,
        price_sync,
    })
}

/// Synchronize only the identities supplied by the just-verified catalog.
///
/// This is a server-owned catalog side effect, not delegated `prices:write`:
/// callers cannot supply a currency, source URL, or price value. The pricing
/// service fixes those to USD and the configured public catalogs and preserves
/// manual prices and last-known prices when a preferred source is unavailable.
async fn sync_discovered_model_prices(
    state: &AppState,
    account_id: Uuid,
    models: &[DiscoveredUpstreamModel],
) -> CatalogPriceSyncResult {
    let price_models = models
        .iter()
        .map(|model| model.model_id.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if price_models.is_empty() {
        return CatalogPriceSyncResult::skipped();
    }
    let sources = crate::pricing::model_price_sources(&state.config);
    match crate::pricing::sync_catalog_model_prices(
        &state.db,
        &state.http,
        price_models,
        &sources,
        state.config.allow_oauth_loopback,
    )
    .await
    {
        Ok(result) => {
            let failed_sources = result
                .source_results
                .into_iter()
                .filter(|source| source.error.is_some())
                .map(|source| source.source)
                .collect::<Vec<_>>();
            CatalogPriceSyncResult {
                status: if failed_sources.is_empty()
                    && result.unmatched.is_empty()
                    && result.candidates.is_empty()
                {
                    "ready"
                } else {
                    "partial"
                },
                currency: "USD",
                imported: result.imported,
                preserved: result.preserved.len(),
                unmatched: result.unmatched.len(),
                ambiguous: result.candidates.len(),
                failed_sources,
                error_code: None,
            }
        }
        Err(error) => {
            tracing::warn!(%account_id, error_category = error.diagnostic_category(),
                "catalog committed but model price synchronization failed");
            CatalogPriceSyncResult {
                status: "error",
                error_code: Some("price_sync_failed"),
                ..CatalogPriceSyncResult::skipped()
            }
        }
    }
}

async fn discover_models(
    state: &AppState,
    account: &crate::provider::UpstreamAccountView,
    credential: &UpstreamCredential,
    blocking: Option<&crate::worker::BlockingTasks>,
    require_complete: bool,
) -> Result<(&'static str, Vec<DiscoveredUpstreamModel>), &'static str> {
    // Native Cursor credentials never enter a compatibility/plugin catalog.
    if account.driver == crate::cursor_native::DRIVER {
        let body = crate::cursor_native::unary(
            state,
            credential,
            crate::cursor_native::Method::UsableModels,
        )
        .await?;
        return crate::cursor_native::models::decode(&body).map(|models| ("cursor_native", models));
    }
    let plugins = state.plugins.clone();
    let driver = account.driver.clone();
    let config = account.config.clone();
    let operation = move || plugins.list_provider_models(&driver, &config);
    let plugin_result = match blocking {
        Some(tasks) => tasks.run(operation).await.ok_or("upstream_unavailable")?,
        None => tokio::task::spawn_blocking(operation)
            .await
            .map_err(|_| "upstream_unavailable")?,
    }
    .map_err(|_| "upstream_unavailable")?;
    if let Some(value) = plugin_result {
        return parse_component_model_catalog(&value, require_complete)
            .map(|models| ("component", models));
    }
    if account.driver == "openai-codex" {
        return discover_codex_models(state, account, credential, require_complete).await;
    }
    if account.driver == crate::provider::antigravity::DRIVER {
        let config = crate::provider::antigravity::Config::from_account(&account.config)
            .map_err(|_| "destination_invalid")?;
        let client = crate::provider::antigravity::NativeClient {
            http: &state.http,
            credential,
            config: &config,
            allow_test_loopback: state.config.allow_oauth_loopback,
        };
        let ids = client
            .list_models()
            .await
            .map_err(|_| "upstream_unavailable")?;
        let models = ids
            .into_iter()
            .filter(|id| id.contains("image"))
            .map(|model_id| DiscoveredUpstreamModel {
                model_id,
                protocol: "generation".into(),
                context_window: None,
                reservation_token_bound: None,
                reservation_bound_source: None,
            })
            .collect();
        return Ok(("antigravity_native", models));
    }
    if account.driver == crate::oauth::managed::kimi::PROVIDER_DRIVER {
        credential
            .validate(unix_millis())
            .map_err(|_| "credential_invalid")?;
        crate::oauth::managed::kimi::validate_credential(credential)
            .map_err(|_| "credential_invalid")?;
        if account.config.get("base_url").and_then(Value::as_str)
            != Some(crate::oauth::managed::kimi::BASE_URL)
        {
            return Err("destination_invalid");
        }
        return Ok(("kimi_builtin", crate::api::kimi_transport::catalog()));
    }
    if !crate::provider::is_openai_compatible_http_driver(&account.driver)
        && !crate::provider::is_new_api_driver(&account.driver)
    {
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
        credential.proxy(),
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
    let started = std::time::Instant::now();
    let response = request.send().await.map_err(|error| {
        let failure = CatalogFailure::transport("send", error.is_timeout(), error.is_connect());
        log_catalog_failure(account, &failure, started);
        failure.code
    })?;
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
    if require_complete
        && (status == StatusCode::PARTIAL_CONTENT
            || response.headers().contains_key(header::CONTENT_RANGE)
            || response
                .headers()
                .get(header::LINK)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.to_ascii_lowercase().contains("next")))
    {
        return Err("partial_catalog");
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
        let chunk = chunk.map_err(|error| {
            let failure = CatalogFailure::transport("body", error.is_timeout(), error.is_connect());
            log_catalog_failure(account, &failure, started);
            failure.code
        })?;
        if body.len().saturating_add(chunk.len()) > MAX_MODEL_CATALOG_BODY {
            return Err("response_too_large");
        }
        body.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&body).map_err(|_| "invalid_response")?;
    if require_complete {
        require_complete_catalog(&value)?;
    }
    let data = value.get("data").ok_or("invalid_response")?;
    parse_model_array(data).map(|models| ("openai_v1", models))
}

async fn discover_codex_models(
    state: &AppState,
    account: &crate::provider::UpstreamAccountView,
    credential: &UpstreamCredential,
    require_complete: bool,
) -> Result<(&'static str, Vec<DiscoveredUpstreamModel>), &'static str> {
    credential
        .validate(unix_millis())
        .map_err(|_| "credential_invalid")?;
    let base_url = validate_config(&account.config).map_err(|_| "destination_invalid")?;
    network::validate_codex_transport(
        &base_url,
        &account.config,
        credential.proxy(),
        state.config.codex_test_loopback,
    )
    .await
    .map_err(|_| "destination_invalid")?;
    let client = state.codex_clients.account_snapshot(account, credential)?;
    let budget = codex_catalog_budget(&account.config)?;
    let account_id = codex_account_header(credential)?;
    let url = format!(
        "{}/models?client_version={}",
        base_url.trim_end_matches('/'),
        crate::oauth::managed::codex::CLIENT_VERSION,
    );
    let (credential_header, credential_value) = credential
        .request_header(unix_millis())
        .map_err(|_| "credential_invalid")?
        .ok_or("credential_invalid")?;
    let mut request = client
        .get(url)
        .default_headers(false)
        .header(credential_header, credential_value)
        .header(header::ACCEPT, "application/json")
        .header(header::ACCEPT_ENCODING, "identity")
        .header(header::USER_AGENT, crate::oauth::managed::codex::USER_AGENT)
        .header("originator", crate::oauth::managed::codex::ORIGINATOR)
        .header("chatgpt-account-id", account_id);
    if let Some((proxy_url, _)) = credential.proxy() {
        request = request.proxy(wreq::Proxy::all(proxy_url).map_err(|_| "destination_invalid")?);
    }
    let started = std::time::Instant::now();
    tracing::info!(account_id = %account.id, credential_generation = account.credential_generation,
        transport = "codex_account_client", total_timeout_ms = budget.total.as_millis() as u64,
        read_timeout_ms = budget.read.as_millis() as u64,
        "upstream model catalog request started");
    let value = bounded_json_response(request, budget, require_complete)
        .await
        .map_err(|failure| {
            log_catalog_failure(account, &failure, started);
            failure.code
        })?;
    if require_complete {
        require_complete_catalog(&value)?;
    }
    let values = value
        .get("models")
        .and_then(Value::as_array)
        .ok_or("invalid_response")?;
    if values.len() > MAX_MODEL_COUNT {
        return Err("invalid_response");
    }
    let configured_models: std::collections::HashSet<String> = state
        .db
        .configured_upstream_model_ids(account.id)
        .await
        .map_err(|_| "upstream_unavailable")?
        .into_iter()
        .collect();
    let normalized = values
        .iter()
        .filter(|value| {
            // Codex's supported_in_api flag filters API-key mode, not
            // ChatGPT OAuth. Visibility controls the picker, not whether
            // authenticated metadata for an explicitly selected slug exists.
            value.get("visibility").and_then(Value::as_str) == Some("list")
                || value
                    .get("slug")
                    .and_then(Value::as_str)
                    .is_some_and(|slug| configured_models.contains(slug))
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
    if normalized.is_empty() {
        // Do not let an empty trusted set reach catalog replacement: that
        // would return a 400 after the sync lease was claimed, leaving the
        // catalog falsely shown as syncing until lease expiry.
        return Err("codex_no_trusted_models");
    }
    parse_discovered_models(normalized).map(|models| ("codex_models", models))
}

fn codex_account_header(credential: &UpstreamCredential) -> Result<String, &'static str> {
    let Some(state) = credential.adapter_state().and_then(Value::as_object) else {
        return Err("credential_invalid");
    };
    if state.len() != 2
        || !matches!(
            state.get("schema").and_then(Value::as_str),
            Some("openai-codex-oauth-v1")
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

async fn bounded_json_response(
    request: wreq::RequestBuilder,
    budget: CatalogBudget,
    require_complete: bool,
) -> Result<Value, CatalogFailure> {
    let deadline = tokio::time::Instant::now() + budget.total;
    let response = tokio::time::timeout_at(deadline, request.send())
        .await
        .map_err(|_| CatalogFailure::transport("send", true, false))?
        .map_err(|error| {
            CatalogFailure::transport("send", error.is_timeout(), error.is_connect())
        })?;
    let status = response.status();
    if status.is_redirection() {
        return Err(CatalogFailure::response("redirect_rejected"));
    }
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(CatalogFailure::response("authentication_failed"));
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(CatalogFailure::response("rate_limited"));
    }
    if !status.is_success() {
        return Err(CatalogFailure::response("upstream_unavailable"));
    }
    if require_complete
        && (status == StatusCode::PARTIAL_CONTENT
            || response.headers().contains_key(header::CONTENT_RANGE)
            || response
                .headers()
                .get(header::LINK)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.to_ascii_lowercase().contains("next")))
    {
        return Err(CatalogFailure::response("partial_catalog"));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MODEL_CATALOG_BODY as u64)
    {
        return Err(CatalogFailure::response("response_too_large"));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    loop {
        let read_deadline = deadline.min(tokio::time::Instant::now() + budget.read);
        let chunk = tokio::time::timeout_at(read_deadline, stream.next())
            .await
            .map_err(|_| CatalogFailure::transport("body", true, false))?;
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk.map_err(|error| {
            CatalogFailure::transport("body", error.is_timeout(), error.is_connect())
        })?;
        if body.len().saturating_add(chunk.len()) > MAX_MODEL_CATALOG_BODY {
            return Err(CatalogFailure::response("response_too_large"));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| CatalogFailure::response("invalid_response"))
}

// We do not follow provider-supplied pagination URLs. A page is not a full
// snapshot and therefore cannot authorize additions or disappearance updates.
fn require_complete_catalog(value: &Value) -> Result<(), &'static str> {
    if value.get("error").is_some_and(|error| !error.is_null())
        || value.get("errors").is_some_and(|errors| {
            !errors.is_null() && errors.as_array().is_none_or(|errors| !errors.is_empty())
        })
        || value
            .get("success")
            .is_some_and(|success| success != &Value::Bool(true))
    {
        return Err("partial_catalog");
    }
    let count = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(Value::as_array)
        .map(Vec::len);
    for object in [Some(value), value.get("pagination"), value.get("meta")]
        .into_iter()
        .flatten()
    {
        let total = object
            .get("total")
            .or_else(|| object.get("total_count"))
            .and_then(Value::as_u64);
        if count
            .zip(total)
            .is_some_and(|(count, total)| total != count as u64)
        {
            return Err("partial_catalog");
        }
        if ["has_more", "hasMore", "partial", "truncated"]
            .iter()
            .any(|key| {
                object
                    .get(key)
                    .is_some_and(|value| value != &Value::Bool(false) && !value.is_null())
            })
            || [
                "next",
                "next_page",
                "next_cursor",
                "next_page_token",
                "continuation_token",
            ]
            .iter()
            .any(|key| {
                object.get(key).is_some_and(|value| {
                    !value.is_null()
                        && value != &Value::String(String::new())
                        && value != &Value::Bool(false)
                })
            })
            || object
                .get("complete")
                .is_some_and(|value| value != &Value::Bool(true))
        {
            return Err("partial_catalog");
        }
    }
    if value
        .get("links")
        .and_then(|links| links.get("next"))
        .is_some_and(|next| !next.is_null() && next.as_str() != Some(""))
    {
        return Err("partial_catalog");
    }
    Ok(())
}

fn parse_component_model_catalog(
    value: &Value,
    require_complete: bool,
) -> Result<Vec<DiscoveredUpstreamModel>, &'static str> {
    // The current plugin ABI exposes only a bare array, with no versioned
    // completeness/continuation contract. Preserve refresh-only support, but
    // never use an unverifiable component snapshot for managed route changes.
    if require_complete {
        return Err("complete_catalog_unsupported");
    }
    parse_model_array(value)
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn managed_pricing_requires_the_reconciled_catalog_to_remain_current() {
        for warning in ["account_changed", "catalog_not_ready", "catalog_changed"] {
            assert!(!managed_route_snapshot_is_current(
                &ManagedModelRouteSyncResult::skipped(warning)
            ));
        }
        let result = ManagedModelRouteSyncResult {
            warnings: vec![
                "operator_route_preserved".into(),
                "unsupported_route_model_or_protocol".into(),
            ],
            ..ManagedModelRouteSyncResult::default()
        };
        assert!(managed_route_snapshot_is_current(&result));
    }

    #[tokio::test]
    async fn manual_and_background_catalog_sync_share_full_catalog_pricing_and_failure_isolation() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

        let server = MockServer::start().await;
        let directory = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::for_test(format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("catalog-pricing.db").display()
        ));
        config.pricing_models_dev_url = format!("{}/models-dev", server.uri());
        config.pricing_litellm_url = format!("{}/litellm", server.uri());
        config.pricing_openrouter_url = format!("{}/openrouter", server.uri());
        let state = AppState::initialize(config).await.unwrap();
        let tenant = "catalog-pricing";
        let account = state
            .db
            .create_upstream_account(
                crate::db::CreateUpstreamAccountInput {
                    tenant_external_id: tenant.into(),
                    name: "catalog-pricing".into(),
                    driver: "http-json".into(),
                    config: json!({"base_url": server.uri()}),
                    credential: UpstreamCredential::None,
                    oauth_session_id: None,
                    oauth_driver: None,
                    oauth_refresh_url: None,
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        state
            .db
            .upsert_model_price("manual", "USD", Decimal::ONE, Decimal::TWO)
            .await
            .unwrap();

        // Cross both the old 100-item response page and the 500-item public pricing
        // limit. Only two models need prices, keeping the regression inexpensive.
        let mut models = (0..500)
            .map(|index| json!({"id": format!("unpriced-{index:03}")}))
            .collect::<Vec<_>>();
        // Discovery accepts 500-byte IDs; pricing must report a long unmatched
        // identity rather than silently dropping it during normalization.
        models[0] = json!({"id": "long-unpriced-".repeat(30)});
        models.extend([json!({"id": "manual"}), json!({"id": "zz-priced-tail"})]);
        Mock::given(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": models})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/models-dev"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"provider": {"models": {
                    "manual": {"cost": {"input": 9, "output": 9}},
                    "zz-priced-tail": {"cost": {"input": 2, "output": 4}}
                }}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let result = sync_account_models(&state, account.id, tenant, None)
            .await
            .unwrap();
        assert_eq!(result.catalog.status, "ready");
        assert_eq!(result.catalog.models.len(), 502);
        assert_eq!(result.price_sync.status, "partial");
        assert_eq!(result.price_sync.imported, 1);
        assert_eq!(result.price_sync.preserved, 1);
        assert_eq!(result.price_sync.unmatched, 500);
        assert_eq!(result.price_sync.failed_sources, ["litellm", "openrouter"]);
        assert_eq!(
            state
                .db
                .model_price_view("manual", "USD")
                .await
                .unwrap()
                .input_per_million,
            "1"
        );
        assert_eq!(
            state
                .db
                .model_price_view("zz-priced-tail", "USD")
                .await
                .unwrap()
                .input_per_million,
            "2"
        );
        server.verify().await;
        server.reset().await;

        Mock::given(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
                {"id": "zz-priced-tail"}
            ]})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/models-dev"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"provider": {"models": {
                    "zz-priced-tail": {"cost": {"input": 3, "output": 6}}
                }}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        sync_upstream_models_after_refresh(&state, account.id, None).await;
        assert_eq!(
            state
                .db
                .model_price_view("zz-priced-tail", "USD")
                .await
                .unwrap()
                .input_per_million,
            "3",
            "background refresh must perform the same price synchronization"
        );
        let catalog = state
            .db
            .upstream_model_catalog(account.id, tenant, None, 10_000)
            .await
            .unwrap();
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.disabled_models.len(), 501);
        let disabled_before = serde_json::to_value(&catalog.disabled_models).unwrap();
        server.verify().await;
        server.reset().await;

        // A failed catalog fetch must not trigger price traffic or turn the
        // last successful directory into an empty/disabled snapshot.
        let failed = sync_account_models(&state, account.id, tenant, None)
            .await
            .unwrap();
        assert_eq!(failed.catalog.status, "stale");
        assert_eq!(failed.price_sync.status, "skipped");
        assert_eq!(failed.catalog.disabled_models.len(), 501);
        let catalog = state
            .db
            .upstream_model_catalog(account.id, tenant, None, 10_000)
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(&catalog.disabled_models).unwrap(),
            disabled_before
        );
        assert_eq!(catalog.models[0].id, "zz-priced-tail");
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/v1/models");
    }

    #[test]
    fn directory_budget_uses_only_explicit_account_timeouts() {
        for config in [
            json!({}),
            json!({"transport_policy":{"connect_attempts":4}}),
        ] {
            assert_eq!(
                codex_catalog_budget(&config).unwrap(),
                CatalogBudget {
                    total: Duration::from_secs(8),
                    read: Duration::from_secs(8)
                }
            );
        }
        let budget = codex_catalog_budget(&json!({"transport_policy":{
            "connect_timeout_millis":1000,"read_timeout_millis":2000,"request_timeout_millis":20000
        }}))
        .unwrap();
        assert_eq!(budget.total, Duration::from_secs(20));
        assert_eq!(budget.read, Duration::from_secs(2));
        assert!(
            codex_catalog_budget(&json!({"transport_policy":{"request_timeout_millis":0}}))
                .is_err()
        );
    }

    async fn pending_catalog(
        budget: CatalogBudget,
    ) -> (
        tokio::net::TcpStream,
        tokio::task::JoinHandle<Result<Value, CatalogFailure>>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/models?private-query-must-not-be-logged=secret",
            listener.local_addr().unwrap()
        );
        let client = crate::build_codex_http_client_with_policy(
            crate::provider::CodexTransportPolicy::default(),
        )
        .unwrap();
        let task = tokio::spawn(bounded_json_response(client.get(url), budget, false));
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = [0; 4096];
        let mut received = 0;
        loop {
            assert!(
                received < bytes.len(),
                "catalog request headers exceed fixture bound"
            );
            let count = socket.read(&mut bytes[received..]).await.unwrap();
            assert_ne!(count, 0, "catalog request ended before complete headers");
            received += count;
            if bytes[..received]
                .windows(4)
                .any(|window| window == b"\r\n\r\n")
            {
                break;
            }
        }
        (socket, task)
    }

    #[tokio::test]
    async fn catalog_classifies_delayed_headers_and_incomplete_body_without_error_urls() {
        let budget = CatalogBudget {
            total: Duration::from_secs(2),
            read: Duration::from_millis(100),
        };
        let (_socket, task) = pending_catalog(budget).await;
        assert_eq!(
            task.await.unwrap().unwrap_err(),
            CatalogFailure::transport("send", true, false)
        );

        let (mut socket, task) = pending_catalog(budget).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(
            task.await.unwrap().unwrap_err(),
            CatalogFailure::transport("body", true, false)
        );

        let (mut socket, task) = pending_catalog(budget).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{")
            .await
            .unwrap();
        socket.shutdown().await.unwrap();
        let failure = task.await.unwrap().unwrap_err();
        assert_eq!(failure, CatalogFailure::transport("body", false, false));
        assert!(!format!("{failure:?}").contains("private-query"));
    }

    #[tokio::test]
    async fn catalog_explicit_budget_allows_headers_after_legacy_deadline() {
        let budget = codex_catalog_budget(&json!({"transport_policy":{
            "request_timeout_millis":20000,"read_timeout_millis":1000
        }}))
        .unwrap();
        let (mut socket, task) = pending_catalog(budget).await;
        // The connection and request are established before advancing time;
        // no real external sleep or flaky elapsed-time comparison is needed.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(9)).await;
        assert!(!task.is_finished());
        tokio::time::resume();
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
            .await
            .unwrap();
        assert_eq!(task.await.unwrap().unwrap(), json!({}));
    }

    #[tokio::test]
    async fn catalog_connection_failure_is_distinct_from_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let client = crate::build_codex_http_client_with_policy(
            crate::provider::CodexTransportPolicy::default(),
        )
        .unwrap();
        let failure = bounded_json_response(
            client.get(format!("http://{address}/models")),
            CatalogBudget {
                total: Duration::from_secs(5),
                read: Duration::from_secs(5),
            },
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(failure, CatalogFailure::transport("send", false, true));
    }

    #[test]
    fn component_catalog_cannot_authorize_managed_route_changes_without_complete_contract() {
        let value = json!([{"id": "a-valid-but-possibly-partial-model"}]);
        assert_eq!(
            parse_component_model_catalog(&value, true).unwrap_err(),
            "complete_catalog_unsupported"
        );
        assert_eq!(
            parse_component_model_catalog(&value, false).unwrap().len(),
            1
        );
    }

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
