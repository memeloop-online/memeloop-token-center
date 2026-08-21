use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::StreamExt;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use crate::{
    db::{BeginOAuthLoginSession, Database, OAuthLoginClaim, OAuthLoginSessionReference},
    error::AppError,
    network::{self, OutboundScope},
    provider::{UpstreamCredential, open_private_json, seal_private_json},
};

use super::OAuthReauthorizationTarget;

pub const PROVIDER_DRIVER: &str = "openai-codex";
pub const LEGACY_PROVIDER_DRIVER: &str = "cpa-codex-oauth";
pub const OAUTH_DRIVER: &str = "openai_codex_device";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
pub const BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

const DEVICE_USER_CODE_ENDPOINT: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_ENDPOINT: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
const DEVICE_TOKEN_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const JWKS_ENDPOINT: &str = "https://auth.openai.com/.well-known/jwks.json";
const EXPECTED_ISSUER: &str = "https://auth.openai.com";
const SESSION_AAD: &[u8] = b"memeloop-token-center/openai-codex-device-login/v1";
const STATE_AAD: &[u8] = b"memeloop-token-center/openai-codex-device-state/v1";
const READY_AAD: &[u8] = b"memeloop-token-center/openai-codex-device-ready/v1";
const RESPONSE_LIMIT: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const SESSION_LIFETIME_MILLIS: i64 = 15 * 60 * 1_000;
const DEFAULT_POLL_SECONDS: u64 = 5;
const MIN_POLL_SECONDS: u64 = 1;
const MAX_POLL_SECONDS: u64 = 60;
const MAX_TOKEN_LIFETIME_SECONDS: i64 = 365 * 24 * 60 * 60;

#[derive(Clone, Debug)]
pub struct StartCodexDeviceLogin {
    pub tenant_external_id: String,
    pub account_name: String,
    pub operator_service_id: Option<Uuid>,
    pub provider_config: Value,
    pub reauthorize: Option<OAuthReauthorizationTarget>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CodexDeviceLoginStart {
    pub driver: &'static str,
    pub verification_url: &'static str,
    pub user_code: String,
    pub session_token: String,
    pub expires_at: i64,
    pub poll_after_seconds: u64,
    pub security_notice: &'static str,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadyCodexDeviceLogin {
    pub session_id: Uuid,
    pub tenant_external_id: String,
    pub account_name: String,
    pub provider_config: Value,
    pub credential: UpstreamCredential,
    pub reauthorize: Option<OAuthReauthorizationTarget>,
}

#[derive(Clone, Debug)]
pub enum CodexDevicePollResult {
    Pending {
        retry_after_seconds: u64,
    },
    Ready {
        lease_owner: Uuid,
        login: Box<ReadyCodexDeviceLogin>,
    },
    Consumed {
        account_id: Uuid,
        tenant_external_id: String,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct CodexDevicePollScope<'a> {
    pub required_tenant: Option<&'a str>,
    pub operator_service_id: Option<Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CodexDeviceLoginState {
    session_id: Uuid,
    tenant_external_id: String,
    account_name: String,
    provider_config: Value,
    operator_service_id: Option<Uuid>,
    device_auth_id: String,
    user_code: String,
    poll_interval_seconds: u64,
    not_before: i64,
    expires_at: i64,
    reauthorize: Option<OAuthReauthorizationTarget>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CodexDeviceSessionToken {
    session_id: Uuid,
    tenant_external_id: String,
    operator_service_id: Option<Uuid>,
    poll_interval_seconds: u64,
    expires_at: i64,
}

#[derive(Clone, Debug)]
struct CodexDeviceEndpoints {
    user_code: String,
    device_token: String,
    verification_url: String,
    token: String,
    jwks: String,
    issuer: String,
}

struct CodexDevicePollRuntime<'a> {
    key_material: &'a [u8],
    now: i64,
    scope: CodexDevicePollScope<'a>,
    allow_test_loopback: bool,
    endpoints: &'a CodexDeviceEndpoints,
}

impl CodexDeviceEndpoints {
    fn production() -> Self {
        Self {
            user_code: DEVICE_USER_CODE_ENDPOINT.to_owned(),
            device_token: DEVICE_TOKEN_ENDPOINT.to_owned(),
            verification_url: DEVICE_VERIFICATION_URL.to_owned(),
            token: TOKEN_ENDPOINT.to_owned(),
            jwks: JWKS_ENDPOINT.to_owned(),
            issuer: EXPECTED_ISSUER.to_owned(),
        }
    }

    #[cfg(test)]
    fn for_test(origin: &str) -> Self {
        Self {
            user_code: format!("{origin}/device/usercode"),
            device_token: format!("{origin}/device/token"),
            verification_url: DEVICE_VERIFICATION_URL.to_owned(),
            token: format!("{origin}/oauth/token"),
            jwks: format!("{origin}/jwks"),
            issuer: EXPECTED_ISSUER.to_owned(),
        }
    }
}

#[derive(Serialize)]
struct DeviceUserCodeRequest<'a> {
    client_id: &'a str,
}

#[derive(Deserialize)]
struct DeviceUserCodeResponse {
    device_auth_id: String,
    #[serde(default)]
    user_code: String,
    #[serde(default)]
    usercode: String,
    #[serde(default)]
    interval: Option<Value>,
}

#[derive(Serialize)]
struct DeviceTokenRequest<'a> {
    device_auth_id: &'a str,
    user_code: &'a str,
}

#[derive(Deserialize)]
struct DeviceTokenResponse {
    authorization_code: String,
    code_verifier: String,
    code_challenge: String,
}

#[derive(Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    refresh_token: String,
    id_token: String,
    expires_in: i64,
}

#[derive(Debug, Deserialize)]
struct VerifiedIdTokenClaims {
    iat: i64,
    sub: String,
    #[serde(rename = "https://api.openai.com/auth")]
    openai_auth: OpenAiAuthClaims,
}

#[derive(Debug, Deserialize)]
struct OpenAiAuthClaims {
    chatgpt_account_id: String,
}

pub async fn start_codex_device_login(
    db: &Database,
    http: &reqwest::Client,
    input: StartCodexDeviceLogin,
    key_material: &[u8],
    now: i64,
    allow_test_loopback: bool,
) -> Result<CodexDeviceLoginStart, AppError> {
    start_codex_device_login_at(
        http,
        db,
        input,
        key_material,
        now,
        allow_test_loopback,
        &CodexDeviceEndpoints::production(),
    )
    .await
}

async fn start_codex_device_login_at(
    http: &reqwest::Client,
    db: &Database,
    input: StartCodexDeviceLogin,
    key_material: &[u8],
    now: i64,
    allow_test_loopback: bool,
    endpoints: &CodexDeviceEndpoints,
) -> Result<CodexDeviceLoginStart, AppError> {
    validate_account_text(&input.tenant_external_id, 200, "tenant")?;
    validate_account_text(input.account_name.trim(), 200, "account name")?;
    let response = post_json(
        http,
        &endpoints.user_code,
        &DeviceUserCodeRequest {
            client_id: CLIENT_ID,
        },
        allow_test_loopback,
    )
    .await?;
    if !response.status().is_success() {
        return Err(device_error());
    }
    let body = bounded_body(response).await.map_err(|_| device_error())?;
    let response: DeviceUserCodeResponse =
        serde_json::from_slice(&body).map_err(|_| device_error())?;
    let user_code = if response.user_code.is_empty() {
        response.usercode
    } else {
        response.user_code
    };
    validate_secret_text(&response.device_auth_id)?;
    validate_user_code(&user_code)?;
    let poll_interval_seconds = parse_poll_seconds(response.interval.as_ref());
    let session_id = Uuid::now_v7();
    let expires_at = now.saturating_add(SESSION_LIFETIME_MILLIS);
    let not_before = now.saturating_add(seconds_millis(poll_interval_seconds));
    let state = CodexDeviceLoginState {
        session_id,
        tenant_external_id: input.tenant_external_id,
        account_name: input.account_name.trim().to_owned(),
        provider_config: input.provider_config,
        operator_service_id: input.operator_service_id,
        device_auth_id: response.device_auth_id,
        user_code: user_code.clone(),
        poll_interval_seconds,
        not_before,
        expires_at,
        reauthorize: input.reauthorize,
    };
    let session = CodexDeviceSessionToken {
        session_id,
        tenant_external_id: state.tenant_external_id.clone(),
        operator_service_id: state.operator_service_id,
        poll_interval_seconds,
        expires_at,
    };
    db.begin_oauth_login_session(BeginOAuthLoginSession {
        session_id,
        flow_kind: "openai_codex_device".to_owned(),
        tenant_external_id: state.tenant_external_id.clone(),
        operator_service_id: state.operator_service_id,
        state_ciphertext: seal_private_json(&state, key_material, STATE_AAD)?,
        next_poll_at: not_before,
        expires_at,
    })
    .await?;
    let verification_url = exact_verification_url(&endpoints.verification_url)?;
    Ok(CodexDeviceLoginStart {
        driver: PROVIDER_DRIVER,
        verification_url,
        user_code,
        session_token: seal_private_json(&session, key_material, SESSION_AAD)?,
        expires_at,
        poll_after_seconds: poll_interval_seconds,
        security_notice: "only_continue_if_you_started_this_login",
    })
}

pub async fn poll_codex_device_login(
    db: &Database,
    http: &reqwest::Client,
    session_token: &str,
    key_material: &[u8],
    now: i64,
    scope: CodexDevicePollScope<'_>,
    allow_test_loopback: bool,
) -> Result<CodexDevicePollResult, AppError> {
    poll_codex_device_login_at(
        http,
        db,
        session_token,
        CodexDevicePollRuntime {
            key_material,
            now,
            scope,
            allow_test_loopback,
            endpoints: &CodexDeviceEndpoints::production(),
        },
    )
    .await
}

async fn poll_codex_device_login_at(
    http: &reqwest::Client,
    db: &Database,
    session_token: &str,
    runtime: CodexDevicePollRuntime<'_>,
) -> Result<CodexDevicePollResult, AppError> {
    let CodexDevicePollRuntime {
        key_material,
        now,
        scope,
        allow_test_loopback,
        endpoints,
    } = runtime;
    let session: CodexDeviceSessionToken =
        open_private_json(session_token, key_material, SESSION_AAD)
            .map_err(|_| AppError::BadRequest("invalid OAuth session token".into()))?;
    if scope
        .required_tenant
        .is_some_and(|tenant| tenant != session.tenant_external_id)
        || session.operator_service_id != scope.operator_service_id
    {
        return Err(AppError::Forbidden);
    }
    if session.expires_at <= now {
        return Err(AppError::BadRequest("OAuth login session expired".into()));
    }
    let reference = OAuthLoginSessionReference {
        session_id: session.session_id,
        flow_kind: "openai_codex_device".to_owned(),
        tenant_external_id: session.tenant_external_id.clone(),
        operator_service_id: session.operator_service_id,
        expires_at: session.expires_at,
    };
    let (lease_owner, state) = match db
        .claim_oauth_login_poll(&reference, now, session.poll_interval_seconds)
        .await?
    {
        OAuthLoginClaim::Pending {
            retry_after_seconds,
        } => {
            return Ok(CodexDevicePollResult::Pending {
                retry_after_seconds,
            });
        }
        OAuthLoginClaim::Consumed { account_id } => {
            return Ok(CodexDevicePollResult::Consumed {
                account_id,
                tenant_external_id: session.tenant_external_id,
            });
        }
        OAuthLoginClaim::Ready {
            lease_owner,
            ready_ciphertext,
        } => {
            let ready = open_private_json(&ready_ciphertext, key_material, READY_AAD)?;
            return Ok(CodexDevicePollResult::Ready {
                lease_owner,
                login: Box::new(ready),
            });
        }
        OAuthLoginClaim::Claimed {
            lease_owner,
            state_ciphertext,
        } => {
            let state: CodexDeviceLoginState =
                open_private_json(&state_ciphertext, key_material, STATE_AAD)?;
            (lease_owner, state)
        }
    };

    let response = post_json(
        http,
        &endpoints.device_token,
        &DeviceTokenRequest {
            device_auth_id: &state.device_auth_id,
            user_code: &state.user_code,
        },
        allow_test_loopback,
    )
    .await?;
    if matches!(response.status().as_u16(), 403 | 404) {
        drop(response);
        db.release_oauth_login_poll(state.session_id, lease_owner, now)
            .await?;
        return Ok(CodexDevicePollResult::Pending {
            retry_after_seconds: state.poll_interval_seconds,
        });
    }
    if !response.status().is_success() {
        let _ = db
            .release_oauth_login_poll(state.session_id, lease_owner, now)
            .await;
        return Err(device_error());
    }
    let body = bounded_body(response).await.map_err(|_| device_error())?;
    let device_token: DeviceTokenResponse =
        serde_json::from_slice(&body).map_err(|_| device_error())?;
    validate_secret_text(&device_token.authorization_code)?;
    validate_secret_text(&device_token.code_verifier)?;
    validate_secret_text(&device_token.code_challenge)?;
    let expected_challenge =
        URL_SAFE_NO_PAD.encode(Sha256::digest(device_token.code_verifier.as_bytes()));
    if expected_challenge != device_token.code_challenge {
        return Err(device_error());
    }

    let token = exchange_authorization_code(
        http,
        &device_token.authorization_code,
        &device_token.code_verifier,
        allow_test_loopback,
        endpoints,
    )
    .await?;
    let claims =
        verify_id_token(http, &token.id_token, now, allow_test_loopback, endpoints).await?;
    validate_secret_text(&token.access_token)?;
    validate_secret_text(&token.refresh_token)?;
    super::managed::account_id(&claims.openai_auth.chatgpt_account_id, "OpenAI Codex")?;
    if !(1..=MAX_TOKEN_LIFETIME_SECONDS).contains(&token.expires_in) {
        return Err(device_error());
    }
    let expires_at = now
        .checked_add(
            token
                .expires_in
                .checked_mul(1_000)
                .ok_or_else(device_error)?,
        )
        .ok_or_else(device_error)?;
    let ready = ReadyCodexDeviceLogin {
        session_id: state.session_id,
        tenant_external_id: state.tenant_external_id,
        account_name: state.account_name,
        provider_config: state.provider_config,
        credential: UpstreamCredential::OAuth {
            access_token: token.access_token,
            refresh_token: Some(token.refresh_token),
            expires_at: Some(expires_at),
            header: "authorization".to_owned(),
            prefix: "Bearer ".to_owned(),
            adapter_state: Some(json!({
                "schema": "openai-codex-oauth-v1",
                "account_id": claims.openai_auth.chatgpt_account_id,
            })),
        },
        reauthorize: state.reauthorize,
    };
    let ready_ciphertext = seal_private_json(&ready, key_material, READY_AAD)?;
    db.stage_oauth_login_ready(state.session_id, lease_owner, ready_ciphertext, now)
        .await?;
    match db
        .claim_oauth_login_poll(&reference, now, session.poll_interval_seconds)
        .await?
    {
        OAuthLoginClaim::Ready {
            lease_owner,
            ready_ciphertext,
        } => Ok(CodexDevicePollResult::Ready {
            lease_owner,
            login: Box::new(open_private_json(
                &ready_ciphertext,
                key_material,
                READY_AAD,
            )?),
        }),
        OAuthLoginClaim::Consumed { account_id } => Ok(CodexDevicePollResult::Consumed {
            account_id,
            tenant_external_id: session.tenant_external_id,
        }),
        OAuthLoginClaim::Pending {
            retry_after_seconds,
        } => Ok(CodexDevicePollResult::Pending {
            retry_after_seconds,
        }),
        OAuthLoginClaim::Claimed { .. } => Err(AppError::Internal),
    }
}

async fn exchange_authorization_code(
    http: &reqwest::Client,
    code: &str,
    verifier: &str,
    allow_test_loopback: bool,
    endpoints: &CodexDeviceEndpoints,
) -> Result<OAuthTokenResponse, AppError> {
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("code", code)
        .append_pair("redirect_uri", DEVICE_TOKEN_REDIRECT_URI)
        .append_pair("code_verifier", verifier)
        .finish();
    let client = oauth_client(http, &endpoints.token, allow_test_loopback).await?;
    let response = client
        .post(&endpoints.token)
        .header(ACCEPT, "application/json")
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|_| device_error())?;
    if !response.status().is_success() {
        return Err(device_error());
    }
    let body = bounded_body(response).await.map_err(|_| device_error())?;
    serde_json::from_slice(&body).map_err(|_| device_error())
}

async fn verify_id_token(
    http: &reqwest::Client,
    id_token: &str,
    now: i64,
    allow_test_loopback: bool,
    endpoints: &CodexDeviceEndpoints,
) -> Result<VerifiedIdTokenClaims, AppError> {
    validate_secret_text(id_token)?;
    let header = decode_header(id_token).map_err(|_| device_error())?;
    if header.alg != Algorithm::RS256 {
        return Err(device_error());
    }
    let kid = header.kid.as_deref().ok_or_else(device_error)?;
    let client = oauth_client(http, &endpoints.jwks, allow_test_loopback).await?;
    let response = client
        .get(&endpoints.jwks)
        .header(ACCEPT, "application/json")
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|_| device_error())?;
    if !response.status().is_success() {
        return Err(device_error());
    }
    let body = bounded_body(response).await.map_err(|_| device_error())?;
    let jwks: JwkSet = serde_json::from_slice(&body).map_err(|_| device_error())?;
    let jwk = jwks.find(kid).ok_or_else(device_error)?;
    let key = DecodingKey::from_jwk(jwk).map_err(|_| device_error())?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_audience(&[CLIENT_ID]);
    validation.set_issuer(&[endpoints.issuer.as_str()]);
    validation.set_required_spec_claims(&["exp", "iat", "iss", "aud", "sub"]);
    validation.leeway = 30;
    let token =
        decode::<VerifiedIdTokenClaims>(id_token, &key, &validation).map_err(|_| device_error())?;
    let now_seconds = now.div_euclid(1_000);
    if token.claims.iat <= 0
        || token.claims.iat > now_seconds.saturating_add(30)
        || token.claims.sub.is_empty()
        || token.claims.sub.len() > 512
        || token.claims.sub.chars().any(char::is_control)
    {
        return Err(device_error());
    }
    Ok(token.claims)
}

async fn post_json<T: Serialize + ?Sized>(
    http: &reqwest::Client,
    endpoint: &str,
    body: &T,
    allow_test_loopback: bool,
) -> Result<reqwest::Response, AppError> {
    let client = oauth_client(http, endpoint, allow_test_loopback).await?;
    client
        .post(endpoint)
        .header(ACCEPT, "application/json")
        .json(body)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|_| device_error())
}

async fn oauth_client(
    http: &reqwest::Client,
    endpoint: &str,
    allow_test_loopback: bool,
) -> Result<reqwest::Client, AppError> {
    network::client_for_url(http, endpoint, OutboundScope::Public, allow_test_loopback)
        .await
        .map_err(|_| device_error())
}

async fn bounded_body(response: reqwest::Response) -> Result<Vec<u8>, AppError> {
    if response
        .content_length()
        .is_some_and(|length| length > RESPONSE_LIMIT as u64)
    {
        return Err(device_error());
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| device_error())?;
        if body.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
            return Err(device_error());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn parse_poll_seconds(value: Option<&Value>) -> u64 {
    let parsed = value
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        })
        .unwrap_or(DEFAULT_POLL_SECONDS);
    parsed.clamp(MIN_POLL_SECONDS, MAX_POLL_SECONDS)
}

fn seconds_millis(seconds: u64) -> i64 {
    i64::try_from(seconds)
        .unwrap_or(i64::MAX)
        .saturating_mul(1_000)
}

fn exact_verification_url(value: &str) -> Result<&'static str, AppError> {
    let parsed = Url::parse(value).map_err(|_| device_error())?;
    if parsed.as_str() != DEVICE_VERIFICATION_URL {
        return Err(device_error());
    }
    Ok(DEVICE_VERIFICATION_URL)
}

fn validate_account_text(value: &str, max: usize, label: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > max
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(AppError::BadRequest(format!("invalid OAuth {label}")));
    }
    Ok(())
}

fn validate_secret_text(value: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > 128 * 1024
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(device_error());
    }
    Ok(())
}

fn validate_user_code(value: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(device_error());
    }
    Ok(())
}

fn device_error() -> AppError {
    AppError::Upstream("OpenAI Codex authorization failed".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, body_string_contains, method, path},
    };

    const TEST_ID_TOKEN: &str = "eyJhbGciOiJSUzI1NiIsImtpZCI6InRlc3Qta2V5IiwidHlwIjoiSldUIn0.eyJpc3MiOiJodHRwczovL2F1dGgub3BlbmFpLmNvbSIsImF1ZCI6ImFwcF9FTW9hbUVFWjczZjBDa1hhWHA3aHJhbm4iLCJzdWIiOiJzdWJqZWN0LXRlc3QiLCJpYXQiOjE3MDAwMDAwMDAsImV4cCI6NDEwMjQ0NDgwMCwiZW1haWwiOiJjb2RleEBleGFtcGxlLnRlc3QiLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjb3VudC10ZXN0In19.QcXeGlgrKbbJTGIIC3a2BHBW_ta6ab_8IGIU7DDmCgp4qhQmiogMJAZ4_nFafd1Ct4H74DNuTcTJbarZrjXZG99pqRVFfDQvkxNpBvydHl6FB2Kn2EbdG-CycoCYQ3Ggx_I9JGiDNNBGO3GhzL82YXbIo4_aJRaXAfKAOgBdcP5CPrK5W_j7UBPpN6v4TqraSWBzABp7P7sdIN6BBNf7FkdZrFyrUpjMP9dTHCJv2S-TI0nJqU_2AcEnj0RSai4yO36SmYkICfA7LRcmP9_W7zmOGCEykXNv7D6PumZ8o1scOthDRV0hUjz8HhMQcdz2il41g3HvjbGZ7Wfm8Efc7g";
    const TEST_RSA_MODULUS: &str = "wIJVXarpR5vXWoza5vjtnxy3XMLY7sYyaxMND0RkeyazN3VIdXVmc1GPMSfjSWixmP0TSLiNxry_2a-aqUqi-qWCeBDcVkYeUDzzEzKdCbGzyoXiWIkh4-3r76CMCBaeiuIucdGGhExiaiIMuFlXCej_b_pQs_rn1RDxVAJnqLIT_mp4llvJ1_gk8B60emxDuDzyGZOVxMqwzY5Z2iL4WpUYZrszwZjFfzvapbmal6QhGVhhCHE_L7MxJUNHA9m-0v6RV-SuuWWusBPkjVmjVzGzDQqGU92WLqdGvS8XHbEEKxOz-j03LrB4q1VHD1VeNOmGaDGPHp96lz9PJPVoqw";

    #[tokio::test]
    async fn mocked_device_protocol_creates_a_verified_ready_result() {
        let server = MockServer::start().await;
        let verifier = "verified-pkce-secret";
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Mock::given(method("POST"))
            .and(path("/device/usercode"))
            .and(body_json(json!({"client_id": CLIENT_ID})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "device_auth_id": "device-auth-test",
                "user_code": "CODEX-TEST",
                "interval": 1
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/device/token"))
            .and(body_json(json!({
                "device_auth_id": "device-auth-test",
                "user_code": "CODEX-TEST"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "authorization_code": "authorization-code-test",
                "code_verifier": verifier,
                "code_challenge": challenge
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains(format!("client_id={CLIENT_ID}")))
            .and(body_string_contains("code=authorization-code-test"))
            .and(body_string_contains(
                "redirect_uri=https%3A%2F%2Fauth.openai.com%2Fdeviceauth%2Fcallback",
            ))
            .and(body_string_contains("code_verifier=verified-pkce-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "access-token-test",
                "refresh_token": "refresh-token-test",
                "id_token": TEST_ID_TOKEN,
                "expires_in": 3600
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "keys": [{
                    "kty": "RSA",
                    "kid": "test-key",
                    "use": "sig",
                    "alg": "RS256",
                    "n": TEST_RSA_MODULUS,
                    "e": "AQAB"
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let directory = tempfile::tempdir().expect("Codex device temporary directory");
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("codex-device.db").display()
        );
        let database = Database::connect(&database_url)
            .await
            .expect("connect Codex device database");
        database
            .migrate()
            .await
            .expect("migrate Codex device database");
        let endpoints = CodexDeviceEndpoints::for_test(&server.uri());
        let now = crate::db::unix_millis();
        let key_material = b"codex-device-test-pepper-at-least-32-bytes";
        let started = start_codex_device_login_at(
            &crate::build_http_client().expect("HTTP client"),
            &database,
            StartCodexDeviceLogin {
                tenant_external_id: "codex-device-test".to_owned(),
                account_name: "Requested account name".to_owned(),
                operator_service_id: None,
                provider_config: json!({
                    "base_url": BASE_URL,
                    "network_scope": "public",
                    "reservation_token_bounds": {}
                }),
                reauthorize: None,
            },
            key_material,
            now,
            true,
            &endpoints,
        )
        .await
        .expect("start device login");
        assert_eq!(started.driver, PROVIDER_DRIVER);
        assert_eq!(started.verification_url, DEVICE_VERIFICATION_URL);
        assert_eq!(started.user_code, "CODEX-TEST");
        assert_eq!(
            started.security_notice,
            "only_continue_if_you_started_this_login"
        );

        let result = poll_codex_device_login_at(
            &crate::build_http_client().expect("HTTP client"),
            &database,
            &started.session_token,
            CodexDevicePollRuntime {
                key_material,
                now: now + 1_000,
                scope: CodexDevicePollScope {
                    required_tenant: Some("codex-device-test"),
                    operator_service_id: None,
                },
                allow_test_loopback: true,
                endpoints: &endpoints,
            },
        )
        .await
        .expect("poll completed device login");
        let ready = match result {
            CodexDevicePollResult::Ready { login, .. } => *login,
            other => panic!("unexpected device result: {other:?}"),
        };
        assert_eq!(ready.account_name, "Requested account name");
        match ready.credential {
            UpstreamCredential::OAuth {
                access_token,
                refresh_token,
                adapter_state,
                ..
            } => {
                assert_eq!(access_token, "access-token-test");
                assert_eq!(refresh_token.as_deref(), Some("refresh-token-test"));
                assert_eq!(
                    adapter_state
                        .as_ref()
                        .and_then(|state| state.get("account_id"))
                        .and_then(Value::as_str),
                    Some("account-test")
                );
            }
            other => panic!("unexpected credential: {other:?}"),
        }
    }
}
