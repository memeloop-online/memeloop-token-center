use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    error::AppError,
    network,
    provider::{
        MANAGED_OAUTH_ADAPTER_API_VERSION, ManagedOAuthAdapterBackend, ProviderCatalog,
        ResolvedManagedOAuthAdapter, UpstreamCredential, validate_config,
    },
};

use super::{bounded_body, endpoint::managed_oauth_endpoint_scope, managed};

const MAX_MANAGED_OAUTH_REQUEST_BYTES: usize = super::MAX_OAUTH_RESPONSE_BYTES + 64 * 1024;
#[cfg(not(test))]
const MANAGED_OAUTH_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(test)]
const MANAGED_OAUTH_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedOAuthNormalizedAccount {
    pub account_name: String,
    pub config: Value,
    pub enabled: bool,
    pub credential: UpstreamCredential,
}
#[derive(Serialize)]
struct ManagedOAuthNormalizeRequest<'a> {
    api_version: &'static str,
    source_type: &'a str,
    payload: &'a Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedOAuthNormalizeResponse {
    api_version: String,
    account: ManagedOAuthNormalizedAccount,
}

#[derive(Serialize)]
struct ManagedOAuthRefreshRequest<'a> {
    api_version: &'static str,
    credential: &'a UpstreamCredential,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedOAuthRefreshResponse {
    api_version: String,
    credential: UpstreamCredential,
}

/// Normalize an opaque CPA managed-OAuth payload with an administrator-owned
/// catalog contribution. The caller cannot provide either destination URL.
pub async fn normalize_managed_oauth_document(
    http: &reqwest::Client,
    adapter: &ResolvedManagedOAuthAdapter,
    payload: &Value,
    allow_test_loopback: bool,
) -> Result<ManagedOAuthNormalizedAccount, AppError> {
    match adapter.backend() {
        ManagedOAuthAdapterBackend::BuiltinCodex => {
            let normalized = managed::codex::normalize(payload)?;
            if normalized.credential.proxy().is_some() {
                network::client_for_config_url(
                    http,
                    &validate_config(&normalized.config)?,
                    &normalized.config,
                    normalized.credential.proxy(),
                    allow_test_loopback,
                )
                .await
                .map_err(|_| AppError::BadRequest("CPA Codex OAuth document is invalid".into()))?;
            }
            return Ok(normalized);
        }
        ManagedOAuthAdapterBackend::BuiltinLegacyGemini => {
            return managed::legacy_gemini::normalize(payload);
        }
        ManagedOAuthAdapterBackend::ReviewedHttp { .. } => {}
    }
    let request = ManagedOAuthNormalizeRequest {
        api_version: MANAGED_OAUTH_ADAPTER_API_VERSION,
        source_type: adapter.source_type(),
        payload,
    };
    let body = serde_json::to_vec(&request)
        .map_err(|_| AppError::BadRequest("managed OAuth payload is invalid".into()))?;
    if body.len() > MAX_MANAGED_OAUTH_REQUEST_BYTES {
        return Err(AppError::BadRequest(
            "managed OAuth payload exceeds its size limit".into(),
        ));
    }
    let endpoint = adapter.normalize_url().ok_or(AppError::Internal)?;
    let response = managed_oauth_adapter_call(http, endpoint, body, allow_test_loopback).await?;
    let response: ManagedOAuthNormalizeResponse = serde_json::from_slice(&response)
        .map_err(|_| AppError::Upstream("managed OAuth adapter returned invalid JSON".into()))?;
    if response.api_version != MANAGED_OAUTH_ADAPTER_API_VERSION
        || response.account.account_name.trim().is_empty()
        || response.account.account_name.len() > 200
        || !matches!(
            response.account.credential,
            UpstreamCredential::OAuth { .. }
        )
    {
        return Err(AppError::Upstream(
            "managed OAuth adapter returned an invalid result".into(),
        ));
    }
    let _ = validate_config(&response.account.config).map_err(|_| {
        AppError::Upstream("managed OAuth adapter returned an invalid result".into())
    })?;
    if let Some(state) = response.account.credential.adapter_state() {
        crate::provider::validate_adapter_state(state).map_err(|_| {
            AppError::Upstream("managed OAuth adapter returned an invalid result".into())
        })?;
    }
    Ok(response.account)
}

pub async fn refresh_managed_oauth_credential(
    http: &reqwest::Client,
    adapter: &ResolvedManagedOAuthAdapter,
    credential: &UpstreamCredential,
    allow_test_loopback: bool,
) -> Result<UpstreamCredential, AppError> {
    if !matches!(credential, UpstreamCredential::OAuth { .. })
        || !credential.has_oauth_refresh_state()
    {
        return Err(AppError::BadRequest(
            "managed OAuth credential has no refresh state".into(),
        ));
    }
    match adapter.backend() {
        ManagedOAuthAdapterBackend::BuiltinCodex => {
            return managed::codex::refresh(http, credential, allow_test_loopback).await;
        }
        ManagedOAuthAdapterBackend::BuiltinLegacyGemini => {
            return managed::legacy_gemini::refresh_unavailable();
        }
        ManagedOAuthAdapterBackend::ReviewedHttp { .. } => {}
    }
    let request = ManagedOAuthRefreshRequest {
        api_version: MANAGED_OAUTH_ADAPTER_API_VERSION,
        credential,
    };
    let body = serde_json::to_vec(&request).map_err(|_| AppError::Internal)?;
    if body.len() > MAX_MANAGED_OAUTH_REQUEST_BYTES {
        return Err(AppError::BadRequest(
            "managed OAuth credential exceeds its size limit".into(),
        ));
    }
    let response =
        managed_oauth_adapter_call(http, adapter.refresh_url(), body, allow_test_loopback).await?;
    let response: ManagedOAuthRefreshResponse = serde_json::from_slice(&response)
        .map_err(|_| AppError::Upstream("managed OAuth adapter returned invalid JSON".into()))?;
    if response.api_version != MANAGED_OAUTH_ADAPTER_API_VERSION
        || !matches!(response.credential, UpstreamCredential::OAuth { .. })
    {
        return Err(AppError::Upstream(
            "managed OAuth adapter returned an invalid result".into(),
        ));
    }
    response
        .credential
        .validate(crate::db::unix_millis())
        .map_err(|_| {
            AppError::Upstream("managed OAuth adapter returned an invalid result".into())
        })?;
    Ok(response.credential)
}

/// Resolve a refresh endpoint from the current catalog and compare the stored
/// snapshot only as consistency evidence. The returned endpoint always comes
/// from the current server/plugin catalog.
pub fn resolve_managed_oauth_refresh_adapter(
    catalog: &ProviderCatalog,
    driver: &str,
    stored_refresh_url: &str,
) -> Result<ResolvedManagedOAuthAdapter, AppError> {
    let adapter = catalog.managed_oauth_adapter_for_driver(driver)?;
    if !adapter.can_refresh() {
        return Err(AppError::BadRequest(
            "managed OAuth adapter does not support refresh".into(),
        ));
    }
    if adapter.api_version() != MANAGED_OAUTH_ADAPTER_API_VERSION
        || adapter.refresh_url() != stored_refresh_url
    {
        return Err(AppError::Conflict(
            "managed OAuth adapter lifecycle metadata no longer matches the active catalog".into(),
        ));
    }
    Ok(adapter)
}

async fn managed_oauth_adapter_call(
    shared_http: &reqwest::Client,
    endpoint: &str,
    body: Vec<u8>,
    allow_test_loopback: bool,
) -> Result<Vec<u8>, AppError> {
    let (url, scope) = managed_oauth_endpoint_scope(endpoint, allow_test_loopback)?;
    let client =
        network::client_for_url(shared_http, url.as_str(), scope, allow_test_loopback).await?;
    let response = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .timeout(MANAGED_OAUTH_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|_| AppError::Upstream("managed OAuth adapter request failed".into()))?;
    if !response.status().is_success() {
        return Err(AppError::Upstream(
            "managed OAuth adapter rejected the request".into(),
        ));
    }
    bounded_body(response).await.map_err(|error| match error {
        AppError::Upstream(_) => {
            AppError::Upstream("managed OAuth adapter response exceeds its limit".into())
        }
        _ => AppError::Internal,
    })
}
