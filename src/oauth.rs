use std::net::IpAddr;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::StreamExt;
use getrandom::fill;
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use sha2::{Digest, Sha256};
use url::{Host, Url};
use uuid::Uuid;

pub mod codex_device;
pub mod managed;

use crate::{
    db::{BeginOAuthLoginSession, Database, OAuthLoginClaim, OAuthLoginSessionReference},
    error::AppError,
    network::{self, OutboundScope},
    provider::{
        MANAGED_OAUTH_ADAPTER_API_VERSION, ManagedOAuthAdapterBackend, ProviderCatalog,
        ResolvedManagedOAuthAdapter, UpstreamCredential, open_private_json, seal_private_json,
        validate_config,
    },
};

const CURSOR_SESSION_AAD: &[u8] = b"memeloop-token-center/cursor-oauth-session/v2";
const CURSOR_STATE_AAD: &[u8] = b"memeloop-token-center/cursor-oauth-state/v2";
const CURSOR_READY_AAD: &[u8] = b"memeloop-token-center/cursor-oauth-ready/v2";
const MAX_OAUTH_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_MANAGED_OAUTH_REQUEST_BYTES: usize = MAX_OAUTH_RESPONSE_BYTES + 64 * 1024;
#[cfg(not(test))]
const MANAGED_OAUTH_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(test)]
const MANAGED_OAUTH_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

pub const DEFAULT_CURSOR_LOGIN_URL: &str = "https://cursor.com/loginDeepControl";
pub const DEFAULT_CURSOR_POLL_URL: &str = "https://api2.cursor.sh/auth/poll";
pub const DEFAULT_CURSOR_REFRESH_URL: &str = "https://api2.cursor.sh/auth/exchange_user_api_key";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CursorOAuthEndpoints {
    #[serde(default = "default_login_url")]
    pub login_url: String,
    #[serde(default = "default_poll_url")]
    pub poll_url: String,
    #[serde(default = "default_refresh_url")]
    pub refresh_url: String,
}

impl Default for CursorOAuthEndpoints {
    fn default() -> Self {
        Self {
            login_url: default_login_url(),
            poll_url: default_poll_url(),
            refresh_url: default_refresh_url(),
        }
    }
}

fn default_login_url() -> String {
    DEFAULT_CURSOR_LOGIN_URL.to_owned()
}

fn default_poll_url() -> String {
    DEFAULT_CURSOR_POLL_URL.to_owned()
}

fn default_refresh_url() -> String {
    DEFAULT_CURSOR_REFRESH_URL.to_owned()
}

#[derive(Clone, Debug)]
pub struct StartCursorLogin {
    pub tenant_external_id: String,
    pub account_name: String,
    pub provider_driver: String,
    pub provider_config: Value,
    pub endpoints: CursorOAuthEndpoints,
    pub oauth_driver: String,
    pub reauthorize: Option<OAuthReauthorizationTarget>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct OAuthReauthorizationTarget {
    pub account_id: Uuid,
    pub expected_updated_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct OAuthLoginStart {
    pub driver: String,
    pub login_url: String,
    pub session_token: String,
    pub expires_at: i64,
    pub poll_after_seconds: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CursorLoginState {
    session_id: Uuid,
    tenant_external_id: String,
    account_name: String,
    provider_driver: String,
    provider_config: Value,
    oauth_driver: String,
    uuid: String,
    verifier: String,
    poll_url: String,
    refresh_url: String,
    expires_at: i64,
    #[serde(default)]
    reauthorize: Option<OAuthReauthorizationTarget>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CursorLoginSessionToken {
    session_id: Uuid,
    tenant_external_id: String,
    operator_service_id: Option<Uuid>,
    expires_at: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReadyCursorLogin {
    pub session_id: Uuid,
    pub tenant_external_id: String,
    pub account_name: String,
    pub provider_driver: String,
    pub provider_config: Value,
    pub oauth_driver: String,
    pub refresh_url: String,
    pub credential: UpstreamCredential,
    pub reauthorize: Option<OAuthReauthorizationTarget>,
}

#[derive(Clone, Debug)]
pub enum CursorPollResult {
    Pending {
        retry_after_seconds: u64,
    },
    Consumed {
        account_id: Uuid,
        tenant_external_id: String,
    },
    Ready {
        lease_owner: Uuid,
        login: Box<ReadyCursorLogin>,
    },
}

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
            return managed::codex::normalize(payload);
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
    let client = match scope {
        OutboundScope::Public => {
            network::client_for_url(shared_http, url.as_str(), scope, allow_test_loopback).await?
        }
        OutboundScope::Private => crate::build_http_client().map_err(|_| AppError::Internal)?,
    };
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

pub async fn start_cursor_login(
    db: &Database,
    input: StartCursorLogin,
    operator_service_id: Option<Uuid>,
    key_material: &[u8],
    now: i64,
) -> Result<OAuthLoginStart, AppError> {
    if input.account_name.trim().is_empty() {
        return Err(AppError::BadRequest(
            "OAuth account name is required".into(),
        ));
    }
    if !matches!(input.oauth_driver.as_str(), "cursor" | "provider_adapter") {
        return Err(AppError::BadRequest(
            "unsupported OAuth adapter protocol".into(),
        ));
    }
    let oauth_driver = input.oauth_driver.clone();
    let _ = validate_config(&input.provider_config)?;
    let (mut login_url, poll_url, refresh_url) = if oauth_driver == "provider_adapter" {
        (
            oauth_adapter_endpoint_scope(&input.endpoints.login_url, "login_url", false)?.0,
            oauth_adapter_endpoint_scope(&input.endpoints.poll_url, "poll_url", false)?.0,
            oauth_adapter_endpoint_scope(&input.endpoints.refresh_url, "refresh_url", false)?.0,
        )
    } else {
        (
            validate_oauth_endpoint(&input.endpoints.login_url, "login_url")?,
            validate_oauth_endpoint(&input.endpoints.poll_url, "poll_url")?,
            validate_oauth_endpoint(&input.endpoints.refresh_url, "refresh_url")?,
        )
    };
    let mut verifier_bytes = [0_u8; 32];
    fill(&mut verifier_bytes).map_err(|_| AppError::Internal)?;
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let uuid = Uuid::now_v7().to_string();
    login_url
        .query_pairs_mut()
        .append_pair("challenge", &challenge)
        .append_pair("uuid", &uuid)
        .append_pair("mode", "login")
        .append_pair("redirectTarget", "cli");
    let expires_at = now.saturating_add(10 * 60 * 1000);
    let session_id = Uuid::now_v7();
    let state = CursorLoginState {
        session_id,
        tenant_external_id: input.tenant_external_id,
        account_name: input.account_name.trim().to_owned(),
        provider_driver: input.provider_driver,
        provider_config: input.provider_config,
        oauth_driver: oauth_driver.clone(),
        uuid,
        verifier,
        poll_url: poll_url.to_string(),
        refresh_url: refresh_url.to_string(),
        expires_at,
        reauthorize: input.reauthorize,
    };
    let session = CursorLoginSessionToken {
        session_id,
        tenant_external_id: state.tenant_external_id.clone(),
        operator_service_id,
        expires_at,
    };
    db.begin_oauth_login_session(BeginOAuthLoginSession {
        session_id,
        tenant_external_id: state.tenant_external_id.clone(),
        operator_service_id,
        state_ciphertext: seal_private_json(&state, key_material, CURSOR_STATE_AAD)?,
        next_poll_at: now,
        expires_at,
    })
    .await?;
    let session_token = seal_private_json(&session, key_material, CURSOR_SESSION_AAD)?;
    Ok(OAuthLoginStart {
        driver: oauth_driver,
        login_url: login_url.to_string(),
        session_token,
        expires_at,
        poll_after_seconds: 1,
    })
}

pub async fn poll_cursor_login(
    db: &Database,
    http: &reqwest::Client,
    session_token: &str,
    key_material: &[u8],
    now: i64,
    required_tenant: Option<&str>,
    operator_service_id: Option<Uuid>,
    allow_test_loopback: bool,
) -> Result<CursorPollResult, AppError> {
    let session: CursorLoginSessionToken =
        open_private_json(session_token, key_material, CURSOR_SESSION_AAD)
            .map_err(|_| AppError::BadRequest("invalid OAuth session token".into()))?;
    if required_tenant.is_some_and(|tenant| tenant != session.tenant_external_id)
        || session.operator_service_id != operator_service_id
    {
        return Err(AppError::Forbidden);
    }
    if session.expires_at <= now {
        return Err(AppError::BadRequest("OAuth login session expired".into()));
    }
    let reference = OAuthLoginSessionReference {
        session_id: session.session_id,
        tenant_external_id: session.tenant_external_id.clone(),
        operator_service_id: session.operator_service_id,
        expires_at: session.expires_at,
    };
    let (lease_owner, state) = match db.claim_oauth_login_poll(&reference, now, 1).await? {
        OAuthLoginClaim::Pending {
            retry_after_seconds,
        } => {
            return Ok(CursorPollResult::Pending {
                retry_after_seconds,
            });
        }
        OAuthLoginClaim::Consumed { account_id } => {
            return Ok(CursorPollResult::Consumed {
                account_id,
                tenant_external_id: session.tenant_external_id,
            });
        }
        OAuthLoginClaim::Ready {
            lease_owner,
            ready_ciphertext,
        } => {
            return Ok(CursorPollResult::Ready {
                lease_owner,
                login: Box::new(open_private_json(
                    &ready_ciphertext,
                    key_material,
                    CURSOR_READY_AAD,
                )?),
            });
        }
        OAuthLoginClaim::Claimed {
            lease_owner,
            state_ciphertext,
        } => (
            lease_owner,
            open_private_json::<CursorLoginState>(
                &state_ciphertext,
                key_material,
                CURSOR_STATE_AAD,
            )?,
        ),
    };
    let (mut poll_url, scope) = if state.oauth_driver == "provider_adapter" {
        oauth_adapter_endpoint_scope(&state.poll_url, "poll_url", allow_test_loopback)?
    } else {
        (
            validate_oauth_endpoint(&state.poll_url, "poll_url")?,
            OutboundScope::Public,
        )
    };
    poll_url
        .query_pairs_mut()
        .append_pair("uuid", &state.uuid)
        .append_pair("verifier", &state.verifier);
    let outbound_http =
        network::client_for_url(http, poll_url.as_str(), scope, allow_test_loopback).await?;
    let response = outbound_http
        .get(poll_url)
        .send()
        .await
        .map_err(AppError::from);
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            let _ = db
                .release_oauth_login_poll(state.session_id, lease_owner, now)
                .await;
            return Err(error);
        }
    };
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        drop(response);
        db.release_oauth_login_poll(state.session_id, lease_owner, now)
            .await?;
        return Ok(CursorPollResult::Pending {
            retry_after_seconds: 1,
        });
    }
    if !response.status().is_success() {
        let status = response.status();
        drop(response);
        let _ = db
            .release_oauth_login_poll(state.session_id, lease_owner, now)
            .await;
        return Err(AppError::Upstream(format!(
            "Cursor OAuth poll returned {}",
            status
        )));
    }
    let body = match bounded_body(response).await {
        Ok(body) => body,
        Err(error) => {
            let _ = db
                .release_oauth_login_poll(state.session_id, lease_owner, now)
                .await;
            return Err(error);
        }
    };
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct CursorTokens {
        access_token: String,
        refresh_token: String,
    }
    let tokens: CursorTokens = serde_json::from_slice(&body)
        .map_err(|_| AppError::Upstream("Cursor OAuth returned invalid JSON".into()))?;
    if tokens.access_token.is_empty() {
        return Err(AppError::Upstream(
            "Cursor OAuth returned an empty access token".into(),
        ));
    }
    let expires_at = token_expiry_millis(&tokens.access_token)
        .unwrap_or_else(|| now.saturating_add(60 * 60 * 1000));
    let ready = ReadyCursorLogin {
        session_id: state.session_id,
        tenant_external_id: state.tenant_external_id,
        account_name: state.account_name,
        provider_driver: state.provider_driver,
        provider_config: state.provider_config,
        oauth_driver: state.oauth_driver,
        refresh_url: state.refresh_url,
        credential: UpstreamCredential::OAuth {
            access_token: tokens.access_token,
            refresh_token: (!tokens.refresh_token.is_empty()).then_some(tokens.refresh_token),
            expires_at: Some(expires_at),
            header: "authorization".to_owned(),
            prefix: "Bearer ".to_owned(),
            adapter_state: None,
        },
        reauthorize: state.reauthorize,
    };
    db.stage_oauth_login_ready(
        ready.session_id,
        lease_owner,
        seal_private_json(&ready, key_material, CURSOR_READY_AAD)?,
        now,
    )
    .await?;
    match db.claim_oauth_login_poll(&reference, now, 1).await? {
        OAuthLoginClaim::Ready {
            lease_owner,
            ready_ciphertext,
        } => Ok(CursorPollResult::Ready {
            lease_owner,
            login: Box::new(open_private_json(
                &ready_ciphertext,
                key_material,
                CURSOR_READY_AAD,
            )?),
        }),
        OAuthLoginClaim::Consumed { account_id } => Ok(CursorPollResult::Consumed {
            account_id,
            tenant_external_id: session.tenant_external_id,
        }),
        OAuthLoginClaim::Pending {
            retry_after_seconds,
        } => Ok(CursorPollResult::Pending {
            retry_after_seconds,
        }),
        OAuthLoginClaim::Claimed { .. } => Err(AppError::Internal),
    }
}

pub async fn refresh_cursor_credential(
    http: &reqwest::Client,
    refresh_url: &str,
    credential: &UpstreamCredential,
    now: i64,
) -> Result<UpstreamCredential, AppError> {
    let UpstreamCredential::OAuth {
        refresh_token: Some(refresh_token),
        ..
    } = credential
    else {
        return Err(AppError::BadRequest(
            "upstream account has no OAuth refresh token".into(),
        ));
    };
    let refresh_url = validate_oauth_endpoint(refresh_url, "refresh_url")?;
    let response = http
        .post(refresh_url)
        .bearer_auth(refresh_token)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body("{}")
        .send()
        .await
        .map_err(|_| AppError::Upstream("Cursor OAuth refresh failed".into()))?;
    if !response.status().is_success() {
        return Err(AppError::Upstream(format!(
            "Cursor OAuth refresh returned {}",
            response.status()
        )));
    }
    let body = bounded_body(response).await?;
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct CursorTokens {
        access_token: String,
        refresh_token: Option<String>,
    }
    let tokens: CursorTokens = serde_json::from_slice(&body)
        .map_err(|_| AppError::Upstream("Cursor OAuth refresh returned invalid JSON".into()))?;
    if tokens.access_token.is_empty() {
        return Err(AppError::Upstream(
            "Cursor OAuth refresh returned an empty access token".into(),
        ));
    }
    let expires_at = token_expiry_millis(&tokens.access_token)
        .unwrap_or_else(|| now.saturating_add(60 * 60 * 1000));
    Ok(UpstreamCredential::OAuth {
        access_token: tokens.access_token,
        refresh_token: tokens
            .refresh_token
            .filter(|value| !value.is_empty())
            .or_else(|| {
                if refresh_token.is_empty() {
                    None
                } else {
                    Some(refresh_token.clone())
                }
            }),
        expires_at: Some(expires_at),
        header: "authorization".to_owned(),
        prefix: "Bearer ".to_owned(),
        adapter_state: credential.adapter_state().cloned(),
    })
}

pub(crate) fn validate_oauth_endpoint(value: &str, field: &str) -> Result<Url, AppError> {
    let url = Url::parse(value)
        .map_err(|_| AppError::BadRequest(format!("OAuth {field} must be a URL")))?;
    let private_http = url.scheme() == "http"
        && url.host().is_some_and(|host| match host {
            Host::Domain(host) => {
                network::is_private_cluster_name(host) || is_loopback_oauth_name(host)
            }
            Host::Ipv4(address) => address.is_loopback(),
            Host::Ipv6(address) => address.is_loopback(),
        });
    if url.scheme() != "https" && !private_http {
        return Err(AppError::BadRequest(format!(
            "OAuth {field} must use HTTPS (explicit cluster HTTP is allowed)"
        )));
    }
    if url.username() != "" || url.password().is_some() || url.fragment().is_some() {
        return Err(AppError::BadRequest(format!(
            "OAuth {field} cannot contain credentials or a fragment"
        )));
    }
    Ok(url)
}

pub(crate) fn validate_oauth_adapter_endpoint(value: &str, field: &str) -> Result<Url, AppError> {
    let url = validate_oauth_endpoint(value, field)?;
    classify_oauth_endpoint(&url, field, false)?;
    Ok(url)
}

pub(crate) fn validate_managed_oauth_adapter_endpoint(
    value: &str,
    field: &str,
) -> Result<Url, AppError> {
    validate_managed_oauth_adapter_endpoint_inner(value, field, false)
}

fn validate_managed_oauth_adapter_endpoint_inner(
    value: &str,
    field: &str,
    allow_test_loopback: bool,
) -> Result<Url, AppError> {
    let url = validate_oauth_endpoint(value, field)?;
    if url.query().is_some() {
        return Err(AppError::BadRequest(format!(
            "OAuth {field} cannot contain a query"
        )));
    }
    classify_oauth_endpoint(&url, field, allow_test_loopback)?;
    Ok(url)
}

fn managed_oauth_endpoint_scope(
    value: &str,
    allow_test_loopback: bool,
) -> Result<(Url, OutboundScope), AppError> {
    let url =
        validate_managed_oauth_adapter_endpoint_inner(value, "adapter_url", allow_test_loopback)?;
    let scope = classify_oauth_endpoint(&url, "adapter_url", allow_test_loopback)?;
    Ok((url, scope))
}

pub(crate) fn oauth_adapter_endpoint_scope(
    value: &str,
    field: &str,
    allow_test_loopback: bool,
) -> Result<(Url, OutboundScope), AppError> {
    let url = validate_oauth_endpoint(value, field)?;
    let scope = classify_oauth_endpoint(&url, field, allow_test_loopback)?;
    Ok((url, scope))
}

fn classify_oauth_endpoint(
    url: &Url,
    field: &str,
    allow_test_loopback: bool,
) -> Result<OutboundScope, AppError> {
    let host = url
        .host()
        .ok_or_else(|| AppError::BadRequest(format!("OAuth {field} must include a host")))?;
    match host {
        Host::Domain(host) if network::is_private_cluster_name(host) => Ok(OutboundScope::Private),
        Host::Domain(_) => Ok(OutboundScope::Public),
        Host::Ipv4(address) => classify_oauth_ip(IpAddr::V4(address), field, allow_test_loopback),
        Host::Ipv6(address) => classify_oauth_ip(IpAddr::V6(address), field, allow_test_loopback),
    }
}

fn classify_oauth_ip(
    address: IpAddr,
    field: &str,
    allow_test_loopback: bool,
) -> Result<OutboundScope, AppError> {
    if allow_test_loopback && address.is_loopback() {
        return Ok(OutboundScope::Private);
    }
    if !network::is_public_ip(address) {
        return Err(AppError::BadRequest(format!(
            "OAuth {field} cannot target a private or reserved IP address"
        )));
    }
    Ok(OutboundScope::Public)
}

fn is_loopback_oauth_name(host: &str) -> bool {
    let normalized = host.trim_end_matches('.').to_ascii_lowercase();
    normalized == "localhost" || normalized.ends_with(".localhost")
}

async fn bounded_body(response: reqwest::Response) -> Result<Vec<u8>, AppError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_OAUTH_RESPONSE_BYTES as u64)
    {
        return Err(AppError::Upstream("OAuth response is too large".into()));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(AppError::from)?;
        if body.len().saturating_add(chunk.len()) > MAX_OAUTH_RESPONSE_BYTES {
            return Err(AppError::Upstream("OAuth response is too large".into()));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn token_expiry_millis(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let payload = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value: Value = serde_json::from_slice(&payload).ok()?;
    value.get("exp")?.as_i64()?.checked_mul(1_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, method, path},
    };

    async fn sqlite_database() -> (tempfile::TempDir, Database) {
        let directory = tempfile::tempdir().expect("OAuth test temporary directory");
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("oauth.db").display()
        );
        let database = Database::connect(&database_url)
            .await
            .expect("connect OAuth test database");
        database
            .migrate()
            .await
            .expect("migrate OAuth test database");
        (directory, database)
    }

    #[tokio::test]
    async fn cursor_login_state_is_encrypted_and_contains_pkce_query() {
        let (_directory, database) = sqlite_database().await;
        let stable_account_id = Uuid::now_v7();
        let started = start_cursor_login(
            &database,
            StartCursorLogin {
                tenant_external_id: "default".to_owned(),
                account_name: "cursor-one".to_owned(),
                provider_driver: "http-json".to_owned(),
                provider_config: json!({"base_url": "https://provider.example"}),
                endpoints: CursorOAuthEndpoints::default(),
                oauth_driver: "cursor".to_owned(),
                reauthorize: Some(OAuthReauthorizationTarget {
                    account_id: stable_account_id,
                    expected_updated_at: 999,
                }),
            },
            None,
            b"test material with at least 32 bytes",
            1_000,
        )
        .await
        .unwrap();
        assert!(started.login_url.contains("challenge="));
        assert!(started.login_url.contains("uuid="));
        assert!(!started.session_token.contains("cursor-one"));
        assert!(
            !started
                .session_token
                .contains(stable_account_id.to_string().as_str())
        );
        assert_eq!(started.expires_at, 601_000);
    }

    #[tokio::test]
    async fn provider_adapter_allows_private_cluster_http_but_rejects_public_http() {
        let (_directory, database) = sqlite_database().await;
        let private = start_cursor_login(
            &database,
            StartCursorLogin {
                tenant_external_id: "default".to_owned(),
                account_name: "plugin-oauth".to_owned(),
                provider_driver: "plugin-provider".to_owned(),
                provider_config: json!({"base_url": "http://plugin-upstream.default.svc"}),
                endpoints: CursorOAuthEndpoints {
                    login_url: "http://oauth-adapter.default.svc/login".to_owned(),
                    poll_url: "http://oauth-adapter.default.svc/poll".to_owned(),
                    refresh_url: "http://oauth-adapter.default.svc/refresh".to_owned(),
                },
                oauth_driver: "provider_adapter".to_owned(),
                reauthorize: None,
            },
            None,
            b"test material with at least 32 bytes",
            1_000,
        )
        .await
        .unwrap();
        assert_eq!(private.driver, "provider_adapter");
        assert!(validate_oauth_endpoint("http://oauth.example.com/login", "login_url").is_err());
        assert_eq!(
            oauth_adapter_endpoint_scope(
                "http://oauth-adapter.default.svc/poll",
                "poll_url",
                false,
            )
            .unwrap()
            .1,
            OutboundScope::Private
        );
        for endpoint in [
            "https://[::1]/oauth",
            "https://[fe80::1]/oauth",
            "https://[fc00::1]/oauth",
            "https://[::ffff:169.254.169.254]/oauth",
            "https://[64:ff9b::a9fe:a9fe]/oauth",
            "https://[2002:7f00:1::]/oauth",
        ] {
            assert!(
                oauth_adapter_endpoint_scope(endpoint, "adapter_url", false).is_err(),
                "interactive adapter accepted {endpoint}"
            );
            assert!(
                validate_managed_oauth_adapter_endpoint(endpoint, "adapter_url").is_err(),
                "managed adapter accepted {endpoint}"
            );
        }
        assert_eq!(
            oauth_adapter_endpoint_scope(
                "https://[2606:4700:4700::1111]/oauth",
                "adapter_url",
                false,
            )
            .unwrap()
            .1,
            OutboundScope::Public
        );
    }

    #[tokio::test]
    async fn cursor_poll_checks_tenant_before_network_io() {
        let (_directory, database) = sqlite_database().await;
        let key_material = b"test material with at least 32 bytes";
        let started = start_cursor_login(
            &database,
            StartCursorLogin {
                tenant_external_id: "tenant-a".to_owned(),
                account_name: "cursor-one".to_owned(),
                provider_driver: "http-json".to_owned(),
                provider_config: json!({"base_url": "https://provider.example"}),
                endpoints: CursorOAuthEndpoints::default(),
                oauth_driver: "cursor".to_owned(),
                reauthorize: None,
            },
            None,
            key_material,
            1_000,
        )
        .await
        .unwrap();

        let error = poll_cursor_login(
            &database,
            &reqwest::Client::new(),
            &started.session_token,
            key_material,
            2_000,
            Some("tenant-b"),
            None,
            true,
        )
        .await
        .expect_err("cross-tenant polling must be rejected before I/O");
        assert!(matches!(error, AppError::Forbidden));
    }

    #[tokio::test]
    async fn cursor_poll_result_is_durable_single_use_and_replayable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/auth/poll"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "accessToken": "cursor-access-token",
                "refreshToken": "cursor-refresh-token"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (_directory, database) = sqlite_database().await;
        let now = crate::db::unix_millis();
        let key_material = b"test material with at least 32 bytes";
        let started = start_cursor_login(
            &database,
            StartCursorLogin {
                tenant_external_id: "cursor-replay".to_owned(),
                account_name: "cursor-primary".to_owned(),
                provider_driver: "http-json".to_owned(),
                provider_config: json!({"base_url": "https://provider.example"}),
                endpoints: CursorOAuthEndpoints {
                    login_url: format!("{}/auth/login", server.uri()),
                    poll_url: format!("{}/auth/poll", server.uri()),
                    refresh_url: format!("{}/auth/refresh", server.uri()),
                },
                oauth_driver: "cursor".to_owned(),
                reauthorize: None,
            },
            None,
            key_material,
            now,
        )
        .await
        .expect("start Cursor login");
        let (lease_owner, ready) = match poll_cursor_login(
            &database,
            &crate::build_http_client().expect("HTTP client"),
            &started.session_token,
            key_material,
            now.saturating_add(1),
            Some("cursor-replay"),
            None,
            true,
        )
        .await
        .expect("poll Cursor login")
        {
            CursorPollResult::Ready { lease_owner, login } => (lease_owner, *login),
            other => panic!("unexpected Cursor result: {other:?}"),
        };
        let account = database
            .create_upstream_account(
                crate::db::CreateUpstreamAccountInput {
                    tenant_external_id: ready.tenant_external_id.clone(),
                    name: ready.account_name,
                    driver: ready.provider_driver,
                    config: ready.provider_config,
                    credential: ready.credential,
                    oauth_session_id: Some(ready.session_id),
                    oauth_driver: Some(ready.oauth_driver),
                    oauth_refresh_url: Some(ready.refresh_url),
                },
                key_material,
            )
            .await
            .expect("create Cursor upstream");
        database
            .finish_oauth_login_session(
                ready.session_id,
                lease_owner,
                account.id,
                now.saturating_add(2),
            )
            .await
            .expect("finish Cursor login");

        match poll_cursor_login(
            &database,
            &crate::build_http_client().expect("HTTP client"),
            &started.session_token,
            key_material,
            now.saturating_add(3),
            Some("cursor-replay"),
            None,
            true,
        )
        .await
        .expect("replay consumed Cursor login")
        {
            CursorPollResult::Consumed { account_id, .. } => {
                assert_eq!(account_id, account.id);
            }
            other => panic!("unexpected replay result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn cursor_refresh_transport_errors_never_echo_the_request_url() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let query_secret = "refresh-query-secret";
        let endpoint = format!("http://{address}/refresh?token={query_secret}");
        let credential = UpstreamCredential::OAuth {
            access_token: "old-access-secret".into(),
            refresh_token: Some("old-refresh-secret".into()),
            expires_at: Some(1),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: None,
        };
        let error = refresh_cursor_credential(
            &crate::build_http_client().unwrap(),
            &endpoint,
            &credential,
            crate::db::unix_millis(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "configured upstream is unavailable: Cursor OAuth refresh failed"
        );
        let rendered = format!("{error:?} {error}");
        let address = address.to_string();
        for secret in [query_secret, "old-refresh-secret", address.as_str()] {
            assert!(!rendered.contains(secret));
        }
    }

    fn managed_adapter(server: &MockServer) -> ResolvedManagedOAuthAdapter {
        ResolvedManagedOAuthAdapter::for_test(
            "managed-mock",
            "codex-test",
            format!("{}/normalize", server.uri()),
            format!("{}/refresh", server.uri()),
        )
    }

    fn assert_managed_error_is_redacted(error: &AppError) {
        let rendered = format!("{error:?} {error}");
        for secret in [
            "source-document-secret",
            "response-body-secret",
            "adapter-token-secret",
            "127.0.0.1",
            "/normalize",
        ] {
            assert!(!rendered.contains(secret), "leaked {secret}: {rendered}");
        }
    }

    #[tokio::test]
    async fn managed_normalize_uses_fixed_protocol_and_returns_bounded_typed_result() {
        let server = MockServer::start().await;
        let adapter = managed_adapter(&server);
        Mock::given(method("POST"))
            .and(path("/normalize"))
            .and(body_json(json!({
                "api_version": MANAGED_OAUTH_ADAPTER_API_VERSION,
                "source_type": "codex-test",
                "payload": {"secret": "source-document-secret"}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "api_version": MANAGED_OAUTH_ADAPTER_API_VERSION,
                "account": {
                    "account_name": "Imported Codex",
                    "config": {"base_url": "https://api.example.test"},
                    "enabled": true,
                    "credential": {
                        "type": "oauth",
                        "access_token": "adapter-token-secret",
                        "refresh_token": "refresh-secret",
                        "expires_at": 4_102_444_800_000_i64,
                        "adapter_state": {"family": "opaque-state"}
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let normalized = normalize_managed_oauth_document(
            &crate::build_http_client().unwrap(),
            &adapter,
            &json!({"secret": "source-document-secret"}),
            true,
        )
        .await
        .unwrap();
        assert_eq!(normalized.account_name, "Imported Codex");
        assert_eq!(
            normalized.credential.adapter_state().unwrap()["family"],
            "opaque-state"
        );
        assert!(!format!("{:?}", normalized.credential).contains("adapter-token-secret"));
    }

    #[tokio::test]
    async fn managed_normalize_never_follows_redirects_or_echoes_failures() {
        let server = MockServer::start().await;
        let adapter = managed_adapter(&server);
        Mock::given(path("/normalize"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/target", server.uri())),
            )
            .mount(&server)
            .await;
        Mock::given(path("/target"))
            .respond_with(ResponseTemplate::new(200).set_body_string("response-body-secret"))
            .expect(0)
            .mount(&server)
            .await;
        let error = normalize_managed_oauth_document(
            &crate::build_http_client().unwrap(),
            &adapter,
            &json!({"secret": "source-document-secret"}),
            true,
        )
        .await
        .unwrap_err();
        assert_managed_error_is_redacted(&error);
    }

    #[tokio::test]
    async fn managed_normalize_rejects_oversize_timeout_and_invalid_json_without_echoing_data() {
        let responses = [
            ResponseTemplate::new(200)
                .set_body_string("response-body-secret".repeat(MAX_OAUTH_RESPONSE_BYTES / 20 + 2)),
            ResponseTemplate::new(200)
                .set_body_string("response-body-secret")
                .set_delay(std::time::Duration::from_millis(500)),
            ResponseTemplate::new(200)
                .set_body_json(json!({"api_version": "wrong", "secret": "response-body-secret"})),
        ];
        for response in responses {
            let server = MockServer::start().await;
            let adapter = managed_adapter(&server);
            Mock::given(path("/normalize"))
                .respond_with(response)
                .mount(&server)
                .await;
            let error = normalize_managed_oauth_document(
                &crate::build_http_client().unwrap(),
                &adapter,
                &json!({"secret": "source-document-secret"}),
                true,
            )
            .await
            .unwrap_err();
            assert_managed_error_is_redacted(&error);
        }
    }

    #[tokio::test]
    async fn managed_refresh_rejects_an_expired_replacement_credential() {
        let server = MockServer::start().await;
        let adapter = managed_adapter(&server);
        Mock::given(method("POST"))
            .and(path("/refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "api_version": MANAGED_OAUTH_ADAPTER_API_VERSION,
                "credential": {
                    "type": "oauth",
                    "access_token": "adapter-token-secret",
                    "refresh_token": "replacement-refresh-secret",
                    "expires_at": crate::db::unix_millis() - 1,
                    "adapter_state": {"family": "response-body-secret"}
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let current: UpstreamCredential = serde_json::from_value(json!({
            "type": "oauth",
            "access_token": "current-access-secret",
            "refresh_token": "current-refresh-secret",
            "expires_at": crate::db::unix_millis() + 60_000
        }))
        .unwrap();
        let error = refresh_managed_oauth_credential(
            &crate::build_http_client().unwrap(),
            &adapter,
            &current,
            true,
        )
        .await
        .unwrap_err();
        assert_managed_error_is_redacted(&error);
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("current-refresh-secret"));
        assert!(!rendered.contains("replacement-refresh-secret"));
    }

    #[test]
    fn managed_refresh_resolution_uses_current_catalog_and_fixed_mismatch_errors() {
        let catalog = ProviderCatalog::builtins();
        let adapter = resolve_managed_oauth_refresh_adapter(
            &catalog,
            "cpa-codex-oauth",
            managed::codex::TOKEN_ENDPOINT,
        )
        .unwrap();
        assert_eq!(adapter.provider_driver(), "cpa-codex-oauth");
        assert_eq!(adapter.backend(), &ManagedOAuthAdapterBackend::BuiltinCodex);
        assert!(adapter.can_refresh());

        let legacy_error = resolve_managed_oauth_refresh_adapter(
            &catalog,
            "cpa-gemini-oauth-legacy",
            managed::legacy_gemini::TOKEN_ENDPOINT,
        )
        .unwrap_err();
        assert_eq!(
            legacy_error.to_string(),
            "invalid request: managed OAuth adapter does not support refresh"
        );

        let stored_secret_url = "https://stored-lifecycle-secret.invalid/token";
        let error =
            resolve_managed_oauth_refresh_adapter(&catalog, "cpa-codex-oauth", stored_secret_url)
                .unwrap_err();
        assert_eq!(
            error.to_string(),
            "conflict: managed OAuth adapter lifecycle metadata no longer matches the active catalog"
        );
        assert!(!format!("{error:?} {error}").contains(stored_secret_url));
    }
}
