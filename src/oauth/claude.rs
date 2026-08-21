use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::StreamExt;
use getrandom::fill;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use url::Url;
use uuid::Uuid;

use crate::{
    db::{BeginOAuthLoginSession, Database, OAuthLoginClaim, OAuthLoginSessionReference},
    error::AppError,
    network::{self, OutboundScope},
    provider::{UpstreamCredential, open_private_json, seal_private_json, validate_config},
};

use super::OAuthReauthorizationTarget;

pub const PROVIDER_DRIVER: &str = "anthropic-claude";
pub const OAUTH_DRIVER: &str = "anthropic_claude_manual_pkce";
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const AUTHORIZE_ENDPOINT: &str = "https://claude.com/cai/oauth/authorize";
pub const TOKEN_ENDPOINT: &str = "https://platform.claude.com/v1/oauth/token";
pub const REVOKE_ENDPOINT: &str = "https://platform.claude.com/v1/oauth/token/revoke";
pub const PROFILE_ENDPOINT: &str = "https://api.anthropic.com/api/oauth/profile";
pub const REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
pub const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

const FLOW_KIND: &str = "claude_manual_pkce";
const SESSION_AAD: &[u8] = b"memeloop-token-center/claude-manual-login/v1";
const STATE_AAD: &[u8] = b"memeloop-token-center/claude-manual-state/v1";
const READY_AAD: &[u8] = b"memeloop-token-center/claude-manual-ready/v1";
const RESPONSE_LIMIT: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const SESSION_LIFETIME_MILLIS: i64 = 10 * 60 * 1_000;
const MAX_TOKEN_LIFETIME_SECONDS: i64 = 365 * 24 * 60 * 60;
const MAX_SECRET_BYTES: usize = 128 * 1024;
pub const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";

#[derive(Clone, Debug)]
pub struct StartClaudeLogin {
    pub tenant_external_id: String,
    pub account_name: String,
    pub operator_service_id: Option<Uuid>,
    pub provider_config: Value,
    pub reauthorize: Option<OAuthReauthorizationTarget>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ClaudeLoginStart {
    pub driver: &'static str,
    pub login_url: String,
    pub session_token: String,
    pub expires_at: i64,
    pub security_notice: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub struct ClaudeCompleteScope<'a> {
    pub required_tenant: Option<&'a str>,
    pub operator_service_id: Option<Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadyClaudeLogin {
    pub session_id: Uuid,
    pub tenant_external_id: String,
    pub account_name: String,
    pub provider_config: Value,
    pub oauth_driver: String,
    pub refresh_url: String,
    pub credential: UpstreamCredential,
    pub reauthorize: Option<OAuthReauthorizationTarget>,
}

#[derive(Clone, Debug)]
pub enum ClaudeCompleteResult {
    Pending {
        retry_after_seconds: u64,
    },
    Ready {
        lease_owner: Uuid,
        login: Box<ReadyClaudeLogin>,
    },
    Consumed {
        account_id: Uuid,
        tenant_external_id: String,
    },
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ClaudeRevokeStatus {
    pub attempted: bool,
    pub revoked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ClaudeLoginState {
    session_id: Uuid,
    tenant_external_id: String,
    account_name: String,
    operator_service_id: Option<Uuid>,
    provider_config: Value,
    verifier: String,
    state: String,
    expires_at: i64,
    reauthorize: Option<OAuthReauthorizationTarget>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ClaudeSessionToken {
    session_id: Uuid,
    flow_kind: String,
    tenant_external_id: String,
    operator_service_id: Option<Uuid>,
    expires_at: i64,
}

#[derive(Clone, Debug)]
struct ClaudeEndpoints {
    authorize: String,
    token: String,
    revoke: String,
    profile: String,
    timeout: Duration,
}

impl ClaudeEndpoints {
    fn production() -> Self {
        Self {
            authorize: AUTHORIZE_ENDPOINT.into(),
            token: TOKEN_ENDPOINT.into(),
            revoke: REVOKE_ENDPOINT.into(),
            profile: PROFILE_ENDPOINT.into(),
            timeout: REQUEST_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn for_test(origin: &str) -> Self {
        Self {
            authorize: format!("{origin}/authorize"),
            token: format!("{origin}/oauth/token"),
            revoke: format!("{origin}/oauth/token/revoke"),
            profile: format!("{origin}/api/oauth/profile"),
            timeout: REQUEST_TIMEOUT,
        }
    }
}

#[derive(Serialize)]
struct AuthorizationCodeGrant<'a> {
    grant_type: &'static str,
    client_id: &'static str,
    code: &'a str,
    state: &'a str,
    redirect_uri: &'static str,
    code_verifier: &'a str,
}

#[derive(Serialize)]
struct RefreshTokenGrant<'a> {
    grant_type: &'static str,
    client_id: &'static str,
    refresh_token: &'a str,
}

#[derive(Serialize)]
struct RevokeTokenRequest<'a> {
    client_id: &'static str,
    token: &'a str,
    token_type_hint: &'static str,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
}

#[derive(Deserialize)]
struct ClaudeProfile {
    account: ClaudeProfileAccount,
}

#[derive(Deserialize)]
struct ClaudeProfileAccount {
    uuid: String,
}

pub async fn start_claude_login(
    db: &Database,
    input: StartClaudeLogin,
    key_material: &[u8],
    now: i64,
) -> Result<ClaudeLoginStart, AppError> {
    start_claude_login_at(db, input, key_material, now, &ClaudeEndpoints::production()).await
}

async fn start_claude_login_at(
    db: &Database,
    input: StartClaudeLogin,
    key_material: &[u8],
    now: i64,
    endpoints: &ClaudeEndpoints,
) -> Result<ClaudeLoginStart, AppError> {
    validate_account_text(&input.tenant_external_id, "tenant")?;
    validate_account_text(input.account_name.trim(), "account name")?;
    let _ = validate_config(&input.provider_config)?;
    let mut verifier_bytes = [0_u8; 32];
    let mut state_bytes = [0_u8; 32];
    fill(&mut verifier_bytes).map_err(|_| AppError::Internal)?;
    fill(&mut state_bytes).map_err(|_| AppError::Internal)?;
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let state_value = URL_SAFE_NO_PAD.encode(state_bytes);
    if verifier == state_value {
        return Err(AppError::Internal);
    }
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut login_url = Url::parse(&endpoints.authorize).map_err(|_| AppError::Internal)?;
    login_url
        .query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state_value);
    let session_id = Uuid::now_v7();
    let expires_at = now.saturating_add(SESSION_LIFETIME_MILLIS);
    let state = ClaudeLoginState {
        session_id,
        tenant_external_id: input.tenant_external_id,
        account_name: input.account_name.trim().to_owned(),
        operator_service_id: input.operator_service_id,
        provider_config: input.provider_config,
        verifier,
        state: state_value,
        expires_at,
        reauthorize: input.reauthorize,
    };
    db.begin_oauth_login_session(BeginOAuthLoginSession {
        session_id,
        flow_kind: FLOW_KIND.into(),
        tenant_external_id: state.tenant_external_id.clone(),
        operator_service_id: state.operator_service_id,
        state_ciphertext: seal_private_json(&state, key_material, STATE_AAD)?,
        next_poll_at: now,
        expires_at,
    })
    .await?;
    let session = ClaudeSessionToken {
        session_id,
        flow_kind: FLOW_KIND.into(),
        tenant_external_id: state.tenant_external_id,
        operator_service_id: state.operator_service_id,
        expires_at,
    };
    Ok(ClaudeLoginStart {
        driver: PROVIDER_DRIVER,
        login_url: login_url.to_string(),
        session_token: seal_private_json(&session, key_material, SESSION_AAD)?,
        expires_at,
        security_notice: "paste_only_the_code_from_the_login_you_started",
    })
}

#[allow(clippy::too_many_arguments)]
pub async fn complete_claude_login(
    db: &Database,
    http: &reqwest::Client,
    session_token: &str,
    code_and_state: &str,
    key_material: &[u8],
    now: i64,
    scope: ClaudeCompleteScope<'_>,
    allow_test_loopback: bool,
) -> Result<ClaudeCompleteResult, AppError> {
    complete_claude_login_at(
        db,
        http,
        session_token,
        code_and_state,
        key_material,
        now,
        scope,
        allow_test_loopback,
        &ClaudeEndpoints::production(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn complete_claude_login_at(
    db: &Database,
    http: &reqwest::Client,
    session_token: &str,
    code_and_state: &str,
    key_material: &[u8],
    now: i64,
    scope: ClaudeCompleteScope<'_>,
    allow_test_loopback: bool,
    endpoints: &ClaudeEndpoints,
) -> Result<ClaudeCompleteResult, AppError> {
    let session: ClaudeSessionToken =
        open_private_json(session_token, key_material, SESSION_AAD)
            .map_err(|_| AppError::BadRequest("invalid OAuth session token".into()))?;
    if session.flow_kind != FLOW_KIND
        || scope
            .required_tenant
            .is_some_and(|tenant| tenant != session.tenant_external_id)
        || session.operator_service_id != scope.operator_service_id
    {
        return Err(AppError::Forbidden);
    }
    if session.expires_at <= now {
        return Err(AppError::BadRequest("OAuth login session expired".into()));
    }
    let (code, supplied_state) = parse_manual_completion(code_and_state)?;
    let reference = OAuthLoginSessionReference {
        session_id: session.session_id,
        flow_kind: FLOW_KIND.into(),
        tenant_external_id: session.tenant_external_id.clone(),
        operator_service_id: session.operator_service_id,
        expires_at: session.expires_at,
    };
    let (lease_owner, state) = match db.claim_oauth_login_poll(&reference, now, 1).await? {
        OAuthLoginClaim::Pending {
            retry_after_seconds,
        } => {
            return Ok(ClaudeCompleteResult::Pending {
                retry_after_seconds,
            });
        }
        OAuthLoginClaim::Consumed { account_id } => {
            return Ok(ClaudeCompleteResult::Consumed {
                account_id,
                tenant_external_id: session.tenant_external_id,
            });
        }
        OAuthLoginClaim::Ready {
            lease_owner,
            ready_ciphertext,
        } => {
            return Ok(ClaudeCompleteResult::Ready {
                lease_owner,
                login: Box::new(open_private_json(
                    &ready_ciphertext,
                    key_material,
                    READY_AAD,
                )?),
            });
        }
        OAuthLoginClaim::Claimed {
            lease_owner,
            state_ciphertext,
        } => {
            let state: ClaudeLoginState =
                open_private_json(&state_ciphertext, key_material, STATE_AAD)?;
            (lease_owner, state)
        }
    };
    if !constant_time_equal(supplied_state.as_bytes(), state.state.as_bytes()) {
        let _ = db
            .release_oauth_login_poll(state.session_id, lease_owner, now)
            .await;
        return Err(AppError::BadRequest("OAuth state did not match".into()));
    }
    let ready = finish_claimed_login(
        http,
        code,
        supplied_state,
        state.clone(),
        now,
        allow_test_loopback,
        endpoints,
    )
    .await;
    let ready = match ready {
        Ok(ready) => ready,
        Err(error) => {
            let _ = db
                .release_oauth_login_poll(state.session_id, lease_owner, now)
                .await;
            return Err(error);
        }
    };
    db.stage_oauth_login_ready(
        ready.session_id,
        lease_owner,
        seal_private_json(&ready, key_material, READY_AAD)?,
        now,
    )
    .await?;
    match db.claim_oauth_login_poll(&reference, now, 1).await? {
        OAuthLoginClaim::Ready {
            lease_owner,
            ready_ciphertext,
        } => Ok(ClaudeCompleteResult::Ready {
            lease_owner,
            login: Box::new(open_private_json(
                &ready_ciphertext,
                key_material,
                READY_AAD,
            )?),
        }),
        OAuthLoginClaim::Consumed { account_id } => Ok(ClaudeCompleteResult::Consumed {
            account_id,
            tenant_external_id: session.tenant_external_id,
        }),
        OAuthLoginClaim::Pending {
            retry_after_seconds,
        } => Ok(ClaudeCompleteResult::Pending {
            retry_after_seconds,
        }),
        OAuthLoginClaim::Claimed { .. } => Err(AppError::Internal),
    }
}

async fn finish_claimed_login(
    http: &reqwest::Client,
    code: &str,
    supplied_state: &str,
    state: ClaudeLoginState,
    now: i64,
    allow_test_loopback: bool,
    endpoints: &ClaudeEndpoints,
) -> Result<ReadyClaudeLogin, AppError> {
    let tokens = post_token_grant(
        http,
        &endpoints.token,
        &AuthorizationCodeGrant {
            grant_type: "authorization_code",
            client_id: CLIENT_ID,
            code,
            state: supplied_state,
            redirect_uri: REDIRECT_URI,
            code_verifier: &state.verifier,
        },
        allow_test_loopback,
        endpoints.timeout,
    )
    .await?;
    validate_token_response(&tokens, true)?;
    let account_id = fetch_profile_account_id(
        http,
        &endpoints.profile,
        &tokens.access_token,
        allow_test_loopback,
        endpoints.timeout,
    )
    .await?;
    let expires_at = expiry_millis(now, tokens.expires_in)?;
    Ok(ReadyClaudeLogin {
        session_id: state.session_id,
        tenant_external_id: state.tenant_external_id,
        account_name: state.account_name,
        provider_config: state.provider_config,
        oauth_driver: OAUTH_DRIVER.into(),
        refresh_url: TOKEN_ENDPOINT.into(),
        credential: UpstreamCredential::OAuth {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at: Some(expires_at),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: Some(json!({
                "schema": "anthropic-claude-oauth-v1",
                "account_id": account_id,
            })),
        },
        reauthorize: state.reauthorize,
    })
}

pub async fn refresh_claude_credential(
    http: &reqwest::Client,
    credential: &UpstreamCredential,
    now: i64,
    allow_test_loopback: bool,
) -> Result<UpstreamCredential, AppError> {
    refresh_claude_credential_at(
        http,
        credential,
        now,
        allow_test_loopback,
        &ClaudeEndpoints::production(),
    )
    .await
}

async fn refresh_claude_credential_at(
    http: &reqwest::Client,
    credential: &UpstreamCredential,
    now: i64,
    allow_test_loopback: bool,
    endpoints: &ClaudeEndpoints,
) -> Result<UpstreamCredential, AppError> {
    let UpstreamCredential::OAuth {
        refresh_token: Some(old_refresh_token),
        adapter_state,
        ..
    } = credential
    else {
        return Err(AppError::BadRequest(
            "upstream account has no OAuth refresh token".into(),
        ));
    };
    validate_secret(old_refresh_token)?;
    let tokens = post_token_grant(
        http,
        &endpoints.token,
        &RefreshTokenGrant {
            grant_type: "refresh_token",
            client_id: CLIENT_ID,
            refresh_token: old_refresh_token,
        },
        allow_test_loopback,
        endpoints.timeout,
    )
    .await?;
    validate_token_response(&tokens, false)?;
    Ok(UpstreamCredential::OAuth {
        access_token: tokens.access_token,
        refresh_token: tokens
            .refresh_token
            .filter(|token| !token.is_empty())
            .or_else(|| Some(old_refresh_token.clone())),
        expires_at: Some(expiry_millis(now, tokens.expires_in)?),
        header: "authorization".into(),
        prefix: "Bearer ".into(),
        adapter_state: adapter_state.clone(),
    })
}

pub async fn revoke_claude_credential(
    http: &reqwest::Client,
    credential: &UpstreamCredential,
    allow_test_loopback: bool,
) -> Result<ClaudeRevokeStatus, AppError> {
    revoke_claude_credential_at(
        http,
        credential,
        allow_test_loopback,
        &ClaudeEndpoints::production(),
    )
    .await
}

async fn revoke_claude_credential_at(
    http: &reqwest::Client,
    credential: &UpstreamCredential,
    allow_test_loopback: bool,
    endpoints: &ClaudeEndpoints,
) -> Result<ClaudeRevokeStatus, AppError> {
    let UpstreamCredential::OAuth { refresh_token, .. } = credential else {
        return Err(AppError::BadRequest(
            "upstream account does not use OAuth".into(),
        ));
    };
    let Some(refresh_token) = refresh_token.as_ref() else {
        return Ok(ClaudeRevokeStatus {
            attempted: false,
            revoked: false,
            status_code: None,
        });
    };
    validate_secret(refresh_token)?;
    let client = match oauth_client(http, &endpoints.revoke, allow_test_loopback).await {
        Ok(client) => client,
        Err(_) => {
            return Ok(ClaudeRevokeStatus {
                attempted: true,
                revoked: false,
                status_code: None,
            });
        }
    };
    let response = client
        .post(&endpoints.revoke)
        .header(ACCEPT, "application/json")
        .header(CONTENT_TYPE, "application/json")
        .json(&RevokeTokenRequest {
            client_id: CLIENT_ID,
            token: refresh_token,
            token_type_hint: "refresh_token",
        })
        .timeout(endpoints.timeout)
        .send()
        .await;
    let Ok(response) = response else {
        return Ok(ClaudeRevokeStatus {
            attempted: true,
            revoked: false,
            status_code: None,
        });
    };
    Ok(ClaudeRevokeStatus {
        attempted: true,
        revoked: response.status().is_success(),
        status_code: Some(response.status().as_u16()),
    })
}

pub fn claude_account_id(credential: &UpstreamCredential) -> Result<Uuid, AppError> {
    let UpstreamCredential::OAuth {
        adapter_state: Some(state),
        ..
    } = credential
    else {
        return Err(AppError::BadRequest(
            "Claude OAuth credential identity is missing".into(),
        ));
    };
    if state.get("schema").and_then(Value::as_str) != Some("anthropic-claude-oauth-v1") {
        return Err(AppError::BadRequest(
            "Claude OAuth credential identity is invalid".into(),
        ));
    }
    state
        .get("account_id")
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or_else(|| AppError::BadRequest("Claude OAuth credential identity is invalid".into()))
}

async fn post_token_grant<T: Serialize + ?Sized>(
    http: &reqwest::Client,
    endpoint: &str,
    grant: &T,
    allow_test_loopback: bool,
    timeout: Duration,
) -> Result<TokenResponse, AppError> {
    let client = oauth_client(http, endpoint, allow_test_loopback).await?;
    let response = client
        .post(endpoint)
        .header(ACCEPT, "application/json")
        .header(CONTENT_TYPE, "application/json")
        .json(grant)
        .timeout(timeout)
        .send()
        .await
        .map_err(|_| claude_error())?;
    if !response.status().is_success() {
        return Err(claude_error());
    }
    let body = bounded_body(response).await?;
    serde_json::from_slice(&body).map_err(|_| claude_error())
}

async fn fetch_profile_account_id(
    http: &reqwest::Client,
    endpoint: &str,
    access_token: &str,
    allow_test_loopback: bool,
    timeout: Duration,
) -> Result<Uuid, AppError> {
    validate_secret(access_token)?;
    let client = oauth_client(http, endpoint, allow_test_loopback).await?;
    let response = client
        .get(endpoint)
        .header(ACCEPT, "application/json")
        .header(AUTHORIZATION, format!("Bearer {access_token}"))
        .header("anthropic-beta", OAUTH_BETA_HEADER)
        .timeout(timeout)
        .send()
        .await
        .map_err(|_| claude_error())?;
    if !response.status().is_success() {
        return Err(claude_error());
    }
    let body = bounded_body(response).await?;
    let profile: ClaudeProfile = serde_json::from_slice(&body).map_err(|_| claude_error())?;
    Uuid::parse_str(&profile.account.uuid).map_err(|_| claude_error())
}

async fn oauth_client(
    http: &reqwest::Client,
    endpoint: &str,
    allow_test_loopback: bool,
) -> Result<reqwest::Client, AppError> {
    network::client_for_url(http, endpoint, OutboundScope::Public, allow_test_loopback)
        .await
        .map_err(|_| claude_error())
}

async fn bounded_body(response: reqwest::Response) -> Result<Vec<u8>, AppError> {
    if response
        .content_length()
        .is_some_and(|length| length > RESPONSE_LIMIT as u64)
    {
        return Err(claude_error());
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| claude_error())?;
        if body.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
            return Err(claude_error());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn parse_manual_completion(value: &str) -> Result<(&str, &str), AppError> {
    let invalid = || AppError::BadRequest("Claude OAuth completion must be code#state".into());
    if value.is_empty()
        || value.len() > MAX_SECRET_BYTES
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(invalid());
    }
    let (code, state) = value.split_once('#').ok_or_else(invalid)?;
    if code.is_empty()
        || state.is_empty()
        || state.contains('#')
        || !code.bytes().all(|byte| byte.is_ascii_graphic())
        || !state.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(invalid());
    }
    Ok((code, state))
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && bool::from(left.ct_eq(right))
}

fn validate_account_text(value: &str, label: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > 200
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(AppError::BadRequest(format!("invalid OAuth {label}")));
    }
    Ok(())
}

fn validate_secret(value: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > MAX_SECRET_BYTES
        || value.trim() != value
        || value.chars().any(char::is_control)
        || reqwest::header::HeaderValue::from_str(&format!("Bearer {value}")).is_err()
    {
        return Err(claude_error());
    }
    Ok(())
}

fn validate_token_response(tokens: &TokenResponse, require_refresh: bool) -> Result<(), AppError> {
    validate_secret(&tokens.access_token)?;
    match tokens.refresh_token.as_deref() {
        Some(token) if !token.is_empty() => validate_secret(token)?,
        Some(_) | None if require_refresh => return Err(claude_error()),
        Some(_) | None => {}
    }
    if !(1..=MAX_TOKEN_LIFETIME_SECONDS).contains(&tokens.expires_in) {
        return Err(claude_error());
    }
    Ok(())
}

fn expiry_millis(now: i64, expires_in: i64) -> Result<i64, AppError> {
    now.checked_add(expires_in.checked_mul(1_000).ok_or_else(claude_error)?)
        .ok_or_else(claude_error)
}

fn claude_error() -> AppError {
    AppError::Upstream("Claude OAuth authorization failed".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_string_contains, header, method, path},
    };

    const KEY: &[u8] = b"claude-oauth-test-key-material-at-least-32-bytes";
    const ACCOUNT_UUID: &str = "719c8604-7a46-4e7d-8fd7-bf6a1be077b5";

    async fn database_url() -> (tempfile::TempDir, String, Database) {
        let directory = tempfile::tempdir().expect("Claude OAuth temporary directory");
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("claude-oauth.db").display()
        );
        let database = Database::connect(&url).await.expect("connect database");
        database.migrate().await.expect("migrate database");
        (directory, url, database)
    }

    fn input(reauthorize: Option<OAuthReauthorizationTarget>) -> StartClaudeLogin {
        StartClaudeLogin {
            tenant_external_id: "claude-test".into(),
            account_name: "Claude primary".into(),
            operator_service_id: None,
            provider_config: json!({
                "base_url": "https://api.anthropic.com",
                "network_scope": "public"
            }),
            reauthorize,
        }
    }

    fn query_value(login_url: &str, key: &str) -> String {
        Url::parse(login_url)
            .unwrap()
            .query_pairs()
            .find_map(|(name, value)| (name == key).then(|| value.into_owned()))
            .unwrap()
    }

    fn credential(refresh_token: Option<&str>) -> UpstreamCredential {
        UpstreamCredential::OAuth {
            access_token: "old-access".into(),
            refresh_token: refresh_token.map(str::to_owned),
            expires_at: Some(1),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: Some(json!({
                "schema": "anthropic-claude-oauth-v1",
                "account_id": ACCOUNT_UUID,
            })),
        }
    }

    async fn mount_success(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string_contains(
                "\"grant_type\":\"authorization_code\"",
            ))
            .and(body_string_contains("\"code\":\"manual-code\""))
            .and(body_string_contains(format!(
                "\"client_id\":\"{CLIENT_ID}\""
            )))
            .and(body_string_contains(format!(
                "\"redirect_uri\":\"{REDIRECT_URI}\""
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "claude-access",
                "refresh_token": "claude-refresh",
                "expires_in": 3600
            })))
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/profile"))
            .and(header("authorization", "Bearer claude-access"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "account": { "uuid": ACCOUNT_UUID, "email_address": "person@example.test" }
            })))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn success_survives_restart_and_is_durable_single_use() {
        let server = MockServer::start().await;
        mount_success(&server).await;
        let (_directory, url, first) = database_url().await;
        let reauth_id = Uuid::now_v7();
        let now = crate::db::unix_millis();
        let endpoints = ClaudeEndpoints::for_test(&server.uri());
        let started = start_claude_login_at(
            &first,
            input(Some(OAuthReauthorizationTarget {
                account_id: reauth_id,
                expected_updated_at: 42,
            })),
            KEY,
            now,
            &endpoints,
        )
        .await
        .unwrap();
        assert_eq!(started.driver, PROVIDER_DRIVER);
        assert!(!started.session_token.contains("Claude primary"));
        let state = query_value(&started.login_url, "state");
        let challenge = query_value(&started.login_url, "code_challenge");
        assert_ne!(
            state, challenge,
            "state must not be derived from the PKCE challenge"
        );
        assert_eq!(query_value(&started.login_url, "scope"), SCOPES);
        assert_eq!(
            query_value(&started.login_url, "redirect_uri"),
            REDIRECT_URI
        );
        drop(first);

        let second = Database::connect(&url).await.expect("reopen database");
        let (lease_owner, ready) = match complete_claude_login_at(
            &second,
            &crate::build_http_client().unwrap(),
            &started.session_token,
            &format!("manual-code#{state}"),
            KEY,
            now + 1,
            ClaudeCompleteScope {
                required_tenant: Some("claude-test"),
                operator_service_id: None,
            },
            true,
            &endpoints,
        )
        .await
        .unwrap()
        {
            ClaudeCompleteResult::Ready { lease_owner, login } => (lease_owner, *login),
            other => panic!("unexpected completion: {other:?}"),
        };
        assert_eq!(
            claude_account_id(&ready.credential).unwrap().to_string(),
            ACCOUNT_UUID
        );
        assert_eq!(ready.reauthorize.as_ref().unwrap().account_id, reauth_id);
        let account = second
            .create_upstream_account(
                crate::db::CreateUpstreamAccountInput {
                    tenant_external_id: ready.tenant_external_id,
                    name: ready.account_name,
                    driver: PROVIDER_DRIVER.into(),
                    config: ready.provider_config,
                    credential: ready.credential,
                    oauth_session_id: Some(ready.session_id),
                    oauth_driver: Some(ready.oauth_driver),
                    oauth_refresh_url: Some(ready.refresh_url),
                },
                KEY,
            )
            .await
            .unwrap();
        second
            .finish_oauth_login_session(ready.session_id, lease_owner, account.id, now + 2)
            .await
            .unwrap();
        match complete_claude_login_at(
            &second,
            &crate::build_http_client().unwrap(),
            &started.session_token,
            &format!("manual-code#{state}"),
            KEY,
            now + 3,
            ClaudeCompleteScope {
                required_tenant: Some("claude-test"),
                operator_service_id: None,
            },
            true,
            &endpoints,
        )
        .await
        .unwrap()
        {
            ClaudeCompleteResult::Consumed { account_id, .. } => assert_eq!(account_id, account.id),
            other => panic!("unexpected replay: {other:?}"),
        }

        let request = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .find(|request| request.url.path() == "/oauth/token")
            .unwrap();
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let verifier = body["code_verifier"].as_str().unwrap();
        assert_eq!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
            challenge
        );
        assert_ne!(verifier, state);
    }

    #[tokio::test]
    async fn state_mismatch_is_rejected_before_exchange_and_scope_is_checked() {
        let server = MockServer::start().await;
        let (_directory, _url, database) = database_url().await;
        let now = crate::db::unix_millis();
        let endpoints = ClaudeEndpoints::for_test(&server.uri());
        let started = start_claude_login_at(&database, input(None), KEY, now, &endpoints)
            .await
            .unwrap();
        let error = complete_claude_login_at(
            &database,
            &crate::build_http_client().unwrap(),
            &started.session_token,
            "manual-code#wrong-state",
            KEY,
            now + 1,
            ClaudeCompleteScope {
                required_tenant: Some("claude-test"),
                operator_service_id: None,
            },
            true,
            &endpoints,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, AppError::BadRequest(_)));
        assert!(server.received_requests().await.unwrap().is_empty());

        let forbidden = complete_claude_login_at(
            &database,
            &crate::build_http_client().unwrap(),
            &started.session_token,
            &format!("manual-code#{}", query_value(&started.login_url, "state")),
            KEY,
            now + 1_001,
            ClaudeCompleteScope {
                required_tenant: Some("another-tenant"),
                operator_service_id: None,
            },
            true,
            &endpoints,
        )
        .await
        .unwrap_err();
        assert!(matches!(forbidden, AppError::Forbidden));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn denial_and_timeout_are_bounded_and_secret_safe() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(
                ResponseTemplate::new(400).set_body_string("denied manual-code response-secret"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let (_directory, _url, database) = database_url().await;
        let now = crate::db::unix_millis();
        let endpoints = ClaudeEndpoints::for_test(&server.uri());
        let started = start_claude_login_at(&database, input(None), KEY, now, &endpoints)
            .await
            .unwrap();
        let error = complete_claude_login_at(
            &database,
            &crate::build_http_client().unwrap(),
            &started.session_token,
            &format!("manual-code#{}", query_value(&started.login_url, "state")),
            KEY,
            now + 1,
            ClaudeCompleteScope {
                required_tenant: Some("claude-test"),
                operator_service_id: None,
            },
            true,
            &endpoints,
        )
        .await
        .unwrap_err();
        let rendered = error.to_string();
        assert_eq!(
            rendered,
            "configured upstream is unavailable: Claude OAuth authorization failed"
        );
        assert!(!rendered.contains("manual-code"));
        assert!(!rendered.contains("response-secret"));

        let slow = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(200)))
            .mount(&slow)
            .await;
        let mut slow_endpoints = ClaudeEndpoints::for_test(&slow.uri());
        slow_endpoints.timeout = Duration::from_millis(25);
        let timeout = refresh_claude_credential_at(
            &crate::build_http_client().unwrap(),
            &credential(Some("refresh-timeout")),
            now,
            true,
            &slow_endpoints,
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(
            timeout,
            "configured upstream is unavailable: Claude OAuth authorization failed"
        );
        assert!(!timeout.contains("refresh-timeout"));
    }

    #[tokio::test]
    async fn refresh_rotates_or_retains_refresh_token_and_identity() {
        let rotated_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string_contains("\"grant_type\":\"refresh_token\""))
            .and(body_string_contains("\"refresh_token\":\"old-refresh\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "rotated-access",
                "refresh_token": "rotated-refresh",
                "expires_in": 7200
            })))
            .expect(1)
            .mount(&rotated_server)
            .await;
        let now = crate::db::unix_millis();
        let rotated = refresh_claude_credential_at(
            &crate::build_http_client().unwrap(),
            &credential(Some("old-refresh")),
            now,
            true,
            &ClaudeEndpoints::for_test(&rotated_server.uri()),
        )
        .await
        .unwrap();
        match &rotated {
            UpstreamCredential::OAuth {
                access_token,
                refresh_token,
                expires_at,
                ..
            } => {
                assert_eq!(access_token, "rotated-access");
                assert_eq!(refresh_token.as_deref(), Some("rotated-refresh"));
                assert_eq!(*expires_at, Some(now + 7_200_000));
            }
            other => panic!("unexpected credential: {other:?}"),
        }
        assert_eq!(
            claude_account_id(&rotated).unwrap().to_string(),
            ACCOUNT_UUID
        );

        let retained_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "retained-access",
                "expires_in": 3600
            })))
            .expect(1)
            .mount(&retained_server)
            .await;
        let retained = refresh_claude_credential_at(
            &crate::build_http_client().unwrap(),
            &credential(Some("keep-refresh")),
            now,
            true,
            &ClaudeEndpoints::for_test(&retained_server.uri()),
        )
        .await
        .unwrap();
        match retained {
            UpstreamCredential::OAuth { refresh_token, .. } => {
                assert_eq!(refresh_token.as_deref(), Some("keep-refresh"))
            }
            other => panic!("unexpected credential: {other:?}"),
        }
    }

    #[tokio::test]
    async fn revoke_is_best_effort_and_never_echoes_secrets() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token/revoke"))
            .and(body_string_contains("\"token\":\"revoke-refresh-secret\""))
            .respond_with(
                ResponseTemplate::new(503).set_body_string("server echoed revoke-refresh-secret"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let status = revoke_claude_credential_at(
            &crate::build_http_client().unwrap(),
            &credential(Some("revoke-refresh-secret")),
            true,
            &ClaudeEndpoints::for_test(&server.uri()),
        )
        .await
        .unwrap();
        assert_eq!(
            status,
            ClaudeRevokeStatus {
                attempted: true,
                revoked: false,
                status_code: Some(503),
            }
        );
        let rendered = format!("{status:?}");
        assert!(!rendered.contains("revoke-refresh-secret"));

        let skipped = revoke_claude_credential_at(
            &crate::build_http_client().unwrap(),
            &credential(None),
            true,
            &ClaudeEndpoints::for_test(&server.uri()),
        )
        .await
        .unwrap();
        assert_eq!(
            skipped,
            ClaudeRevokeStatus {
                attempted: false,
                revoked: false,
                status_code: None,
            }
        );
    }

    #[test]
    fn manual_completion_parser_is_strict() {
        assert_eq!(
            parse_manual_completion("code#state").unwrap(),
            ("code", "state")
        );
        for invalid in [
            "",
            "code",
            "#state",
            "code#",
            "code#state#extra",
            " code#state",
            "code#state\n",
        ] {
            assert!(
                parse_manual_completion(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
    }
}
