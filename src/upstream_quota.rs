//! Supplier quota projection; mutations live in the explicit durable reset workflow.
mod kimi;
mod normalize;
pub(crate) mod reset;

use std::{collections::HashMap, sync::Arc, time::Duration};

use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, Semaphore};
use uuid::Uuid;

use crate::{
    AppState,
    db::unix_millis,
    provider::{UpstreamAccountView, UpstreamCredential},
};

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const CREDITS_URL: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
const FRESH_MS: i64 = 30_000;
const STALE_MS: i64 = 300_000;
const MAX_ENTRIES: usize = 128;
const BODY_LIMIT: usize = 1024 * 1024;

#[derive(Clone, Copy)]
struct QuotaRequestContext {
    account_id: Uuid,
    credential_generation: i64,
    endpoint_kind: &'static str,
}

impl QuotaRequestContext {
    fn for_account(account: &UpstreamAccountView, endpoint_kind: &'static str) -> Self {
        Self {
            account_id: account.id,
            credential_generation: account.credential_generation,
            endpoint_kind,
        }
    }
}

fn quota_reqwest_error_kind(
    is_timeout: bool,
    is_connect: bool,
    is_body: bool,
    is_request: bool,
) -> &'static str {
    if is_timeout {
        "timeout"
    } else if is_connect {
        "connect"
    } else if is_body {
        "body"
    } else if is_request {
        "request"
    } else {
        "other"
    }
}

fn quota_reqwest_error_code(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "quota_timeout"
    } else {
        "quota_transport_failed"
    }
}

fn log_quota_request_error(
    context: QuotaRequestContext,
    phase: &'static str,
    error: &reqwest::Error,
    started: tokio::time::Instant,
) {
    // Do not log reqwest's error display/chain: it can include the complete
    // URL, including proxy userinfo or request-derived credentials. These
    // predicates are the deliberately allowlisted diagnostic surface.
    tracing::warn!(
        operation = "quota_supplier_read",
        upstream_account_id = %context.account_id,
        credential_generation = context.credential_generation,
        endpoint_kind = context.endpoint_kind,
        phase,
        error_kind = quota_reqwest_error_kind(
            error.is_timeout(),
            error.is_connect(),
            error.is_body(),
            error.is_request(),
        ),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "quota supplier request failed"
    );
}
#[derive(Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    account: Uuid,
    tenant: Uuid,
    external_tenant: String,
    generation: i64,
    updated_at: i64,
}

impl CacheKey {
    fn new(account: &UpstreamAccountView, tenant: &str) -> Self {
        Self {
            account: account.id,
            tenant: account.tenant_id,
            external_tenant: tenant.to_owned(),
            generation: account.credential_generation,
            updated_at: account.updated_at,
        }
    }
}

#[derive(Clone, Serialize)]
pub(crate) struct QuotaSnapshot {
    contract_version: &'static str,
    upstream_account_id: Uuid,
    tenant_external_id: String,
    provider: String,
    status: &'static str,
    observed_at: Option<i64>,
    stale_after: Option<i64>,
    stale: bool,
    freshness: &'static str,
    plan_type: Option<String>,
    /// No current native adapter has a verified supplier workspace field.
    workspace: Option<String>,
    capabilities: QuotaCapabilities,
    /// Subscription lifetime is not OAuth token lifetime. Unknown stays null.
    subscription_active_until: Option<i64>,
    reset_credits: Vec<ResetCredit>,
    windows: Vec<QuotaWindow>,
    credits: Credits,
    reset_capability: ResetCapability,
    error_code: Option<&'static str>,
}

#[derive(Clone, Serialize)]
struct QuotaWindow {
    id: String,
    label: String,
    used_percent: Option<f64>,
    used: Option<f64>,
    remaining: Option<f64>,
    limit: Option<f64>,
    /// Supplier-declared unit only. Numeric values without one remain unknown.
    unit: Option<String>,
    reset_at: Option<i64>,
    period_seconds: Option<i64>,
    source: &'static str,
    reset_is_estimated: bool,
    allowed: Option<bool>,
    limit_reached: Option<bool>,
}

#[derive(Clone, Serialize)]
struct QuotaCapabilities {
    read: bool,
    plan: bool,
    workspace: bool,
    window_amounts: bool,
    window_amount_unit: bool,
    window_percent: bool,
    reset_credit_expiry: bool,
    subscription_expiry: bool,
    /// These describe the quota GET itself, not the separately confirmed reset.
    supplier_read_only: bool,
    refreshes_credentials: bool,
    consumes_reset_credit: bool,
}

impl QuotaCapabilities {
    fn for_provider(provider: &str) -> Self {
        Self {
            read: matches!(provider, "openai-codex" | "kimi-oauth"),
            plan: provider == "openai-codex",
            workspace: false,
            window_amounts: provider == "kimi-oauth",
            window_amount_unit: false,
            window_percent: matches!(provider, "openai-codex" | "kimi-oauth"),
            reset_credit_expiry: provider == "openai-codex",
            subscription_expiry: false,
            supplier_read_only: true,
            refreshes_credentials: false,
            consumes_reset_credit: false,
        }
    }
}

#[derive(Clone, Serialize)]
struct ResetCredit {
    status: Option<String>,
    granted_at: Option<i64>,
    expires_at: Option<i64>,
    source: &'static str,
}

#[derive(Clone, Default, Serialize)]
struct Credits {
    balance: Option<String>,
    unlimited: Option<bool>,
    has_credits: Option<bool>,
    source: Option<&'static str>,
}

#[derive(Clone, Serialize)]
struct ResetCapability {
    provider_supported: Option<bool>,
    implementation_available: bool,
    /// Preparing performs only read-only checks. Keep it retryable after a
    /// transient refresh failure instead of turning that failure into a
    /// permanent product capability decision.
    prepare_available: bool,
    confirmation_required: bool,
    retryable: bool,
    available_credits: Option<i64>,
    applicable_credits: Option<i64>,
    reason: &'static str,
    credit_error_code: Option<&'static str>,
    /// Server-held driver capability; fresh credit evidence is still required.
    evidence: &'static str,
}

impl QuotaSnapshot {
    fn empty(account: &UpstreamAccountView, tenant: &str, error: Option<&'static str>) -> Self {
        let codex = account.driver == "openai-codex";
        let known_read_adapter = matches!(account.driver.as_str(), "openai-codex" | "kimi-oauth");
        Self {
            contract_version: "upstream_quota_v1",
            upstream_account_id: account.id,
            tenant_external_id: tenant.to_owned(),
            provider: account.driver.clone(),
            status: if QuotaCapabilities::for_provider(&account.driver).read {
                "error"
            } else {
                "unsupported"
            },
            observed_at: None,
            stale_after: None,
            stale: false,
            freshness: "unobserved",
            plan_type: None,
            workspace: None,
            capabilities: QuotaCapabilities::for_provider(&account.driver),
            subscription_active_until: None,
            reset_credits: Vec::new(),
            windows: Vec::new(),
            credits: Credits::default(),
            reset_capability: ResetCapability {
                provider_supported: if codex {
                    Some(true)
                } else if account.driver == "kimi-oauth" {
                    Some(false)
                } else {
                    None
                },
                implementation_available: codex,
                prepare_available: codex,
                confirmation_required: codex,
                retryable: codex,
                available_credits: None,
                applicable_credits: None,
                reason: if codex {
                    "quota_refresh_required"
                } else {
                    "quota_reset_not_supported"
                },
                credit_error_code: None,
                evidence: if known_read_adapter {
                    "server_driver_contract"
                } else {
                    "unknown_provider"
                },
            },
            error_code: error,
        }
    }
}

#[derive(Default)]
struct Cached {
    value: Option<QuotaSnapshot>,
    refresh_after: i64,
}

#[derive(Default)]
struct Entry {
    cached: Mutex<Cached>,
    flight: Mutex<()>,
}

pub(crate) struct QuotaCache {
    entries: Mutex<HashMap<CacheKey, Arc<Entry>>>,
    permits: Semaphore,
}

impl Default for QuotaCache {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            permits: Semaphore::new(4),
        }
    }
}

impl QuotaCache {
    pub(crate) async fn read(
        &self,
        state: &AppState,
        account: &UpstreamAccountView,
        credential: &UpstreamCredential,
        tenant: &str,
    ) -> QuotaSnapshot {
        let empty = |error| QuotaSnapshot::empty(account, tenant, error);
        if !QuotaCapabilities::for_provider(&account.driver).read {
            return empty(None);
        }
        // Tenant is the authorized endpoint's canonical external ID. Include it
        // even though account IDs are global: rename must not reuse old labels.
        let key = CacheKey::new(account, tenant);
        let entry = {
            let mut entries = self.entries.lock().await;
            if !entries.contains_key(&key) && entries.len() >= MAX_ENTRIES {
                let evict = entries
                    .iter()
                    .find(|(_, entry)| Arc::strong_count(entry) == 1)
                    .map(|(key, _)| key.clone());
                if let Some(evict) = evict {
                    entries.remove(&evict);
                } else {
                    return empty(Some("quota_busy"));
                }
            }
            entries.entry(key).or_default().clone()
        };
        let now = unix_millis();
        let previous = {
            let cached = entry.cached.lock().await;
            if now < cached.refresh_after
                && let Some(value) = &cached.value
            {
                return value.clone();
            }
            cached.value.clone()
        };
        let fallback = |error| stale_or_error(previous.clone(), empty(Some(error)), now);
        let Ok(_flight) = entry.flight.try_lock() else {
            return fallback("quota_refresh_in_progress");
        };
        // A task can finish between our first read and acquiring flight.
        {
            let cached = entry.cached.lock().await;
            if unix_millis() < cached.refresh_after
                && let Some(value) = &cached.value
            {
                return value.clone();
            }
        }
        let Ok(_permit) = self.permits.try_acquire() else {
            return fallback("quota_busy");
        };
        // Includes DNS/proxy setup, both GETs and bounded body decoding.
        let refresh_started = tokio::time::Instant::now();
        let result = match tokio::time::timeout(Duration::from_secs(8), async {
            if account.driver == "kimi-oauth" {
                kimi::read(state, credential, empty(None)).await
            } else {
                read_codex(state, account, credential, empty(None)).await
            }
        })
        .await
        {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!(
                    operation = "quota_supplier_read",
                    upstream_account_id = %account.id,
                    credential_generation = account.credential_generation,
                    endpoint_kind = "read",
                    phase = "overall",
                    error_kind = "timeout",
                    elapsed_ms = refresh_started.elapsed().as_millis() as u64,
                    "quota supplier request timed out"
                );
                Err("quota_timeout")
            }
        };
        let mut value = match result {
            Ok(mut value) => {
                value.finalize_reset_capability();
                value
            }
            Err(error) => fallback(error),
        };
        if value.error_code.is_some() && value.reset_capability.implementation_available {
            value.reset_capability.retryable = value.reset_capability.implementation_available;
            value.reset_capability.prepare_available =
                value.reset_capability.implementation_available;
            value.reset_capability.reason = "quota_refresh_failed_retryable";
        }
        let mut cached = entry.cached.lock().await;
        cached.refresh_after = unix_millis()
            + if value.error_code.is_none() {
                FRESH_MS
            } else {
                10_000
            };
        cached.value = Some(value.clone());
        value
    }
}

fn stale_or_error(
    previous: Option<QuotaSnapshot>,
    empty: QuotaSnapshot,
    now: i64,
) -> QuotaSnapshot {
    match previous {
        Some(mut value)
            if value
                .observed_at
                .is_some_and(|at| now.saturating_sub(at) <= STALE_MS) =>
        {
            value.stale = true;
            value.freshness = "stale";
            value.error_code = empty.error_code;
            value.reset_capability.retryable = value.reset_capability.implementation_available;
            value.reset_capability.prepare_available =
                value.reset_capability.implementation_available;
            if value.reset_capability.implementation_available {
                value.reset_capability.reason = "quota_refresh_failed_retryable";
            }
            value
        }
        _ => empty,
    }
}

async fn read_codex(
    state: &AppState,
    account: &UpstreamAccountView,
    credential: &UpstreamCredential,
    mut snapshot: QuotaSnapshot,
) -> Result<QuotaSnapshot, &'static str> {
    let observation_started_at = unix_millis();
    let recovery_fence = match state
        .db
        .upstream_quota_recovery_fence(account.id, account.credential_generation)
        .await
    {
        Ok(fence) => fence,
        Err(_) => {
            tracing::warn!(
                upstream_account_id = %account.id,
                credential_generation = account.credential_generation,
                "failed to capture quota recovery fence; quota read remains read-only"
            );
            None
        }
    };
    credential
        .validate(observation_started_at)
        .map_err(|_| "credential_invalid")?;
    let account_header = crate::oauth::managed::codex::account_header_value(credential)
        .map_err(|_| "credential_invalid")?;
    // Never use caller/account base_url or network_scope for these fixed
    // supplier endpoints. Only the already-authorized encrypted proxy is reused.
    let http = crate::network::client_for_codex_url(
        &state.http,
        USAGE_URL,
        &json!({"network_scope":"public"}),
        credential.proxy(),
        false,
    )
    .await
    .map_err(|_| {
        tracing::warn!(
            operation = "quota_supplier_read",
            upstream_account_id = %account.id,
            credential_generation = account.credential_generation,
            endpoint_kind = "quota_client",
            phase = "client",
            error_kind = "destination_invalid",
            "quota supplier client setup failed"
        );
        "quota_destination_invalid"
    })?;
    let (usage, reset) = tokio::join!(
        get_json(
            &http,
            credential,
            account_header.clone(),
            USAGE_URL,
            QuotaRequestContext::for_account(account, "usage"),
        ),
        get_json(
            &http,
            credential,
            account_header,
            CREDITS_URL,
            QuotaRequestContext::for_account(account, "credits"),
        ),
    );
    let observed_at = unix_millis();
    normalize::usage(&mut snapshot, &usage?, observed_at)?;
    match reset {
        Ok(reset) => {
            if let Err(error) = normalize::reset_credits(&mut snapshot, &reset, observed_at) {
                snapshot.reset_capability.credit_error_code = Some(error);
            }
        }
        Err(error) => snapshot.reset_capability.credit_error_code = Some(error),
    }
    snapshot.status = "ready";
    snapshot.freshness = "fresh";
    snapshot.observed_at = Some(observed_at);
    snapshot.stale_after = Some(observed_at + FRESH_MS);
    snapshot.finalize_reset_capability();
    if snapshot.conclusively_allows_codex()
        && let Some(recovery_fence) = recovery_fence
    {
        match state
            .db
            .recover_upstream_quota_from_observation(
                account.id,
                account.credential_generation,
                recovery_fence,
            )
            .await
        {
            Ok(true) => tracing::info!(
                upstream_account_id = %account.id,
                credential_generation = account.credential_generation,
                observation_started_at,
                observed_at,
                "fresh quota evidence cleared an exhausted upstream cooldown"
            ),
            Ok(false) => {}
            Err(_) => tracing::warn!(
                upstream_account_id = %account.id,
                credential_generation = account.credential_generation,
                error_code = "quota_health_recovery_failed",
                "failed to apply fresh quota recovery evidence"
            ),
        }
    }
    Ok(snapshot)
}

impl QuotaSnapshot {
    fn conclusively_allows_codex(&self) -> bool {
        self.status == "ready"
            && !self.stale
            && self.error_code.is_none()
            && self.windows.iter().any(|window| {
                window.id.starts_with("code:")
                    && window.allowed == Some(true)
                    && window.limit_reached != Some(true)
            })
    }

    fn finalize_reset_capability(&mut self) {
        let capability = &mut self.reset_capability;
        if capability.provider_supported != Some(true) || !capability.implementation_available {
            capability.prepare_available = false;
            capability.confirmation_required = false;
            capability.retryable = false;
            return;
        }
        capability.confirmation_required = true;
        if capability.credit_error_code.is_some() {
            capability.prepare_available = true;
            capability.retryable = true;
            capability.reason = "reset_credit_refresh_failed_retryable";
        } else if capability.available_credits.unwrap_or_default() >= 1
            && capability.applicable_credits.unwrap_or_default() >= 1
        {
            capability.prepare_available = true;
            capability.retryable = false;
            capability.reason = "explicit_confirmation_required";
        } else if capability.available_credits.is_some() && capability.applicable_credits.is_some()
        {
            capability.prepare_available = false;
            capability.retryable = false;
            capability.reason = "no_applicable_reset_credits";
        } else {
            capability.prepare_available = true;
            capability.retryable = true;
            capability.reason = "quota_refresh_required";
        }
    }
}

async fn get_json(
    http: &reqwest::Client,
    credential: &UpstreamCredential,
    account: reqwest::header::HeaderValue,
    url: &str,
    context: QuotaRequestContext,
) -> Result<Value, &'static str> {
    let started = tokio::time::Instant::now();
    let mut request = http
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(
            reqwest::header::USER_AGENT,
            crate::oauth::managed::codex::USER_AGENT,
        )
        .header("chatgpt-account-id", account)
        .header(
            "originator",
            if context.endpoint_kind == "credits" {
                "Codex Desktop"
            } else {
                crate::oauth::managed::codex::ORIGINATOR
            },
        )
        .timeout(Duration::from_secs(6));
    if context.endpoint_kind == "credits" {
        request = request.header("openai-beta", "codex-1");
    }
    let response = credential
        .apply(request, unix_millis())
        .map_err(|_| "credential_invalid")?
        .send()
        .await
        .map_err(|error| {
            log_quota_request_error(context, "send", &error, started);
            quota_reqwest_error_code(&error)
        })?;
    decode_response(response, context, started).await
}

async fn decode_response(
    response: reqwest::Response,
    context: QuotaRequestContext,
    started: tokio::time::Instant,
) -> Result<Value, &'static str> {
    if !response.status().is_success() {
        return Err(match response.status().as_u16() {
            401 | 403 => "quota_not_authorized",
            429 => "quota_rate_limited",
            _ => "quota_upstream_error",
        });
    }
    if response
        .content_length()
        .is_some_and(|len| len > BODY_LIMIT as u64)
    {
        return Err("quota_response_too_large");
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            log_quota_request_error(context, "body", &error, started);
            quota_reqwest_error_code(&error)
        })?;
        if bytes.len().saturating_add(chunk.len()) > BODY_LIMIT {
            return Err("quota_response_too_large");
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| "quota_invalid_payload")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    #[test]
    fn cache_identity_separates_tenant_rename_and_credential_generation() {
        let mut account: UpstreamAccountView = serde_json::from_value(json!({
            "id":Uuid::from_u128(1), "tenant_id":Uuid::from_u128(2), "name":"fixture",
            "driver":"kimi-oauth", "auth_kind":"oauth", "connection_method":"native_oauth",
            "credential_generation":1, "status":"active", "config":{}, "can_refresh":true,
            "can_rotate":false, "can_reauthorize":true, "route_count":0, "created_at":0, "updated_at":10
        })).unwrap();
        let key = CacheKey::new(&account, "before-rename");
        let renamed = CacheKey::new(&account, "after-rename");
        account.credential_generation += 1;
        let rotated = CacheKey::new(&account, "before-rename");
        account.tenant_id = Uuid::from_u128(3);
        let other_tenant = CacheKey::new(&account, "before-rename");
        let entries = HashMap::from([
            (key.clone(), 1),
            (renamed.clone(), 2),
            (rotated.clone(), 3),
            (other_tenant.clone(), 4),
        ]);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries.get(&key), Some(&1));
        assert_eq!(entries.get(&renamed), Some(&2));
        assert_eq!(entries.get(&rotated), Some(&3));
        assert_eq!(entries.get(&other_tenant), Some(&4));
        let mut previous = QuotaSnapshot::empty(&account, "after-rename", None);
        previous.observed_at = Some(1000);
        previous.status = "ready";
        let fallback = stale_or_error(
            Some(previous.clone()),
            QuotaSnapshot::empty(&account, "after-rename", Some("quota_timeout")),
            1001,
        );
        assert_eq!(fallback.tenant_external_id, "after-rename");
        assert_eq!(fallback.freshness, "stale");
        assert_eq!(fallback.error_code, Some("quota_timeout"));
        assert_eq!(
            fallback.reset_capability.reason,
            "quota_reset_not_supported"
        );
        let expired = stale_or_error(
            Some(previous),
            QuotaSnapshot::empty(&account, "after-rename", Some("quota_timeout")),
            STALE_MS + 1001,
        );
        assert!(expired.observed_at.is_none());
        assert_eq!(expired.status, "error");
    }

    #[test]
    fn first_read_failure_retains_server_reset_capability_without_consuming() {
        let account: UpstreamAccountView = serde_json::from_value(json!({
            "id":Uuid::from_u128(1), "tenant_id":Uuid::from_u128(2), "name":"fixture",
            "driver":"openai-codex", "auth_kind":"oauth", "connection_method":"native_oauth",
            "credential_generation":1, "status":"active", "config":{}, "can_refresh":true,
            "can_rotate":false, "can_reauthorize":true, "route_count":0, "created_at":0, "updated_at":10
        })).unwrap();
        let first_read_failure =
            QuotaSnapshot::empty(&account, "tenant", Some("quota_transport_failed"));
        assert_eq!(first_read_failure.freshness, "unobserved");
        assert_eq!(
            first_read_failure.reset_capability.provider_supported,
            Some(true)
        );
        assert!(first_read_failure.reset_capability.implementation_available);
        assert!(first_read_failure.reset_capability.prepare_available);
        assert!(first_read_failure.reset_capability.confirmation_required);
        assert_eq!(
            first_read_failure.reset_capability.evidence,
            "server_driver_contract"
        );
        assert!(first_read_failure.capabilities.supplier_read_only);
        assert!(!first_read_failure.capabilities.refreshes_credentials);
        assert!(!first_read_failure.capabilities.consumes_reset_credit);
        assert!(first_read_failure.workspace.is_none());
    }

    #[test]
    fn quota_reqwest_classification_keeps_timeouts_distinct_from_transport() {
        assert_eq!(quota_reqwest_error_kind(true, true, true, true), "timeout");
        assert_eq!(quota_reqwest_error_kind(false, true, true, true), "connect");
        assert_eq!(quota_reqwest_error_kind(false, false, true, true), "body");
        assert_eq!(
            quota_reqwest_error_kind(false, false, false, true),
            "request"
        );
        assert_eq!(
            quota_reqwest_error_kind(false, false, false, false),
            "other"
        );
    }

    #[tokio::test]
    async fn quota_transport_only_gets_and_does_not_publish_error_bodies() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/usage"))
            .and(header("authorization", "Bearer fixture-token"))
            .and(header("chatgpt-account-id", "fixture-account"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"plan_type":"pro"})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/credits"))
            .and(header("openai-beta", "codex-1"))
            .respond_with(ResponseTemplate::new(403).set_body_string("secret upstream body"))
            .expect(1)
            .mount(&server)
            .await;
        let credential = UpstreamCredential::OAuth {
            access_token: "fixture-token".into(),
            refresh_token: None,
            expires_at: None,
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: None,
            proxy_url: None,
            proxy_network_scope: None,
        };
        let http = crate::build_http_client().unwrap();
        let account = reqwest::header::HeaderValue::from_static("fixture-account");
        let usage_context = QuotaRequestContext {
            account_id: Uuid::from_u128(1),
            credential_generation: 2,
            endpoint_kind: "usage",
        };
        let credits_context = QuotaRequestContext {
            endpoint_kind: "credits",
            ..usage_context
        };
        assert_eq!(
            get_json(
                &http,
                &credential,
                account.clone(),
                &format!("{}/usage", server.uri()),
                usage_context
            )
            .await
            .unwrap()["plan_type"],
            "pro"
        );
        assert_eq!(
            get_json(
                &http,
                &credential,
                account,
                &format!("{}/credits", server.uri()),
                credits_context
            )
            .await
            .unwrap_err(),
            "quota_not_authorized"
        );
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|request| request.method.as_str() == "GET")
        );
    }

    #[tokio::test]
    async fn cache_has_a_four_account_global_bound_and_singleflight_entry() {
        let cache = QuotaCache::default();
        let first = cache.permits.try_acquire_many(4).unwrap();
        assert!(cache.permits.try_acquire().is_err());
        drop(first);
        assert!(cache.permits.try_acquire().is_ok());
        let entry = Entry::default();
        let flight = entry.flight.try_lock().unwrap();
        assert!(entry.flight.try_lock().is_err());
        drop(flight);
        assert!(entry.flight.try_lock().is_ok());
    }
}
