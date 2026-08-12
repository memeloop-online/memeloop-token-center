use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::StreamExt;
use getrandom::fill;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use crate::{
    error::AppError,
    provider::{UpstreamCredential, open_private_json, seal_private_json, validate_config},
};

const CURSOR_LOGIN_AAD: &[u8] = b"memeloop-token-center/cursor-oauth-login/v1";
const SUBSCRIPTION_BRIDGE_LOGIN_AAD: &[u8] =
    b"memeloop-token-center/subscription-bridge-oauth-login/v1";
const MAX_OAUTH_RESPONSE_BYTES: usize = 1024 * 1024;

pub const DEFAULT_CURSOR_LOGIN_URL: &str = "https://cursor.com/loginDeepControl";
pub const DEFAULT_CURSOR_POLL_URL: &str = "https://api2.cursor.sh/auth/poll";
pub const DEFAULT_CURSOR_REFRESH_URL: &str = "https://api2.cursor.sh/auth/exchange_user_api_key";

#[derive(Clone, Debug, Deserialize, Serialize)]
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
    uuid: String,
    verifier: String,
    poll_url: String,
    refresh_url: String,
    expires_at: i64,
}

#[derive(Clone, Debug)]
pub struct ReadyCursorLogin {
    pub session_id: Uuid,
    pub tenant_external_id: String,
    pub account_name: String,
    pub provider_driver: String,
    pub provider_config: Value,
    pub credential: UpstreamCredential,
}

#[derive(Clone, Debug)]
pub enum CursorPollResult {
    Pending { retry_after_seconds: u64 },
    Ready(Box<ReadyCursorLogin>),
}

#[derive(Clone, Debug)]
pub struct StartSubscriptionBridgeLogin {
    pub tenant_external_id: String,
    pub account_name: String,
    pub provider_config: Value,
    pub bridge_secret: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SubscriptionBridgeLoginState {
    session_id: Uuid,
    tenant_external_id: String,
    account_name: String,
    provider_config: Value,
    bridge_secret: Option<String>,
    bridge_state: String,
    expires_at: i64,
}

#[derive(Clone, Debug)]
pub struct ReadySubscriptionBridgeLogin {
    pub session_id: Uuid,
    pub tenant_external_id: String,
    pub account_name: String,
    pub provider_config: Value,
    pub credential: UpstreamCredential,
}

#[derive(Clone, Debug)]
pub enum SubscriptionBridgePollResult {
    Pending {
        retry_after_seconds: u64,
        message: Option<String>,
    },
    Ready(Box<ReadySubscriptionBridgeLogin>),
}

pub async fn start_subscription_bridge_login(
    http: &reqwest::Client,
    input: StartSubscriptionBridgeLogin,
    key_material: &[u8],
    now: i64,
) -> Result<OAuthLoginStart, AppError> {
    if input.account_name.trim().is_empty() {
        return Err(AppError::BadRequest(
            "OAuth account name is required".into(),
        ));
    }
    let base_url = validate_config(&input.provider_config)?;
    let provider = subscription_provider(&input.provider_config)?.to_owned();
    validate_optional_bridge_secret(input.bridge_secret.as_deref())?;
    let response = subscription_bridge_call(
        http,
        &base_url,
        input.bridge_secret.as_deref(),
        "/v1/oauth/start",
        &json!({"provider": provider.as_str()}),
    )
    .await?;
    let login_url = response
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::Upstream("subscription bridge returned no login URL".into()))?;
    let login_url = validate_oauth_endpoint(login_url, "login_url")?.to_string();
    let bridge_state = response
        .get("state")
        .and_then(Value::as_str)
        .filter(|state| !state.is_empty())
        .ok_or_else(|| AppError::Upstream("subscription bridge returned no login state".into()))?
        .to_owned();
    let expires_at = now.saturating_add(15 * 60 * 1_000);
    let state = SubscriptionBridgeLoginState {
        session_id: Uuid::now_v7(),
        tenant_external_id: input.tenant_external_id,
        account_name: input.account_name.trim().to_owned(),
        provider_config: input.provider_config,
        bridge_secret: input.bridge_secret,
        bridge_state,
        expires_at,
    };
    Ok(OAuthLoginStart {
        driver: format!("subscription-bridge:{provider}"),
        login_url,
        session_token: seal_private_json(&state, key_material, SUBSCRIPTION_BRIDGE_LOGIN_AAD)?,
        expires_at,
        poll_after_seconds: 1,
    })
}

pub async fn poll_subscription_bridge_login(
    http: &reqwest::Client,
    session_token: &str,
    key_material: &[u8],
    now: i64,
) -> Result<SubscriptionBridgePollResult, AppError> {
    let state: SubscriptionBridgeLoginState =
        open_private_json(session_token, key_material, SUBSCRIPTION_BRIDGE_LOGIN_AAD)
            .map_err(|_| AppError::BadRequest("invalid OAuth session token".into()))?;
    if state.expires_at <= now {
        return Err(AppError::BadRequest("OAuth login session expired".into()));
    }
    let base_url = validate_config(&state.provider_config)?;
    let provider = subscription_provider(&state.provider_config)?.to_owned();
    let response = subscription_bridge_call(
        http,
        &base_url,
        state.bridge_secret.as_deref(),
        "/v1/oauth/poll",
        &json!({"provider": provider.as_str(), "state": state.bridge_state}),
    )
    .await?;
    let message = response
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match response.get("status").and_then(Value::as_str) {
        Some("success") => {
            let auth = response
                .get("auth")
                .and_then(Value::as_object)
                .ok_or_else(|| AppError::Upstream("subscription bridge returned no auth".into()))?;
            if auth.get("upstream").and_then(Value::as_str) != Some(provider.as_str())
                || auth.get("type").and_then(Value::as_str) != Some("subscription-bridge")
            {
                return Err(AppError::Upstream(
                    "subscription bridge returned mismatched auth metadata".into(),
                ));
            }
            let handle = auth
                .get("handle")
                .and_then(Value::as_str)
                .filter(|handle| !handle.is_empty())
                .ok_or_else(|| {
                    AppError::Upstream("subscription bridge returned no account handle".into())
                })?
                .to_owned();
            let credential = UpstreamCredential::SubscriptionBridge {
                handle,
                secret: state.bridge_secret,
            };
            credential.validate(now)?;
            Ok(SubscriptionBridgePollResult::Ready(Box::new(
                ReadySubscriptionBridgeLogin {
                    session_id: state.session_id,
                    tenant_external_id: state.tenant_external_id,
                    account_name: state.account_name,
                    provider_config: state.provider_config,
                    credential,
                },
            )))
        }
        Some("error") => {
            Err(AppError::Upstream(message.unwrap_or_else(|| {
                "subscription bridge OAuth failed".to_owned()
            })))
        }
        _ => Ok(SubscriptionBridgePollResult::Pending {
            retry_after_seconds: 1,
            message,
        }),
    }
}

fn subscription_provider(config: &Value) -> Result<&str, AppError> {
    match config.get("provider").and_then(Value::as_str) {
        Some(provider @ ("copilot" | "cursor")) => Ok(provider),
        _ => Err(AppError::BadRequest(
            "subscription bridge config.provider must be copilot or cursor".into(),
        )),
    }
}

fn validate_optional_bridge_secret(secret: Option<&str>) -> Result<(), AppError> {
    if let Some(secret) = secret {
        if secret.is_empty() {
            return Err(AppError::BadRequest(
                "subscription bridge secret cannot be empty".into(),
            ));
        }
        reqwest::header::HeaderValue::from_str(&format!("Bearer {secret}"))
            .map_err(|_| AppError::BadRequest("invalid subscription bridge secret".into()))?;
    }
    Ok(())
}

async fn subscription_bridge_call(
    http: &reqwest::Client,
    base_url: &str,
    secret: Option<&str>,
    path: &str,
    body: &Value,
) -> Result<Value, AppError> {
    let request = http.post(format!("{base_url}{path}")).json(body);
    let request = match secret {
        Some(secret) => request.bearer_auth(secret),
        None => request,
    };
    let response = request
        .send()
        .await
        .map_err(|error| AppError::Upstream(format!("subscription bridge failed: {error}")))?;
    let status = response.status();
    let body = bounded_body(response).await?;
    if !status.is_success() {
        return Err(AppError::Upstream(format!(
            "subscription bridge returned HTTP {}",
            status.as_u16()
        )));
    }
    serde_json::from_slice(&body)
        .map_err(|_| AppError::Upstream("subscription bridge returned invalid JSON".into()))
}

pub fn start_cursor_login(
    mut input: StartCursorLogin,
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
    let mut login_url = validate_oauth_endpoint(&input.endpoints.login_url, "login_url")?;
    let poll_url = validate_oauth_endpoint(&input.endpoints.poll_url, "poll_url")?;
    let refresh_url = validate_oauth_endpoint(&input.endpoints.refresh_url, "refresh_url")?;
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
    if let Some(config) = input.provider_config.as_object_mut() {
        config.insert(
            "oauth".to_owned(),
            json!({
                "driver": oauth_driver.clone(),
                "refresh_url": refresh_url.as_str()
            }),
        );
    }
    let state = CursorLoginState {
        session_id: Uuid::now_v7(),
        tenant_external_id: input.tenant_external_id,
        account_name: input.account_name.trim().to_owned(),
        provider_driver: input.provider_driver,
        provider_config: input.provider_config,
        uuid,
        verifier,
        poll_url: poll_url.to_string(),
        refresh_url: refresh_url.to_string(),
        expires_at,
    };
    let session_token = seal_private_json(&state, key_material, CURSOR_LOGIN_AAD)?;
    Ok(OAuthLoginStart {
        driver: oauth_driver,
        login_url: login_url.to_string(),
        session_token,
        expires_at,
        poll_after_seconds: 1,
    })
}

pub async fn poll_cursor_login(
    http: &reqwest::Client,
    session_token: &str,
    key_material: &[u8],
    now: i64,
) -> Result<CursorPollResult, AppError> {
    let state: CursorLoginState = open_private_json(session_token, key_material, CURSOR_LOGIN_AAD)
        .map_err(|_| AppError::BadRequest("invalid OAuth session token".into()))?;
    if state.expires_at <= now {
        return Err(AppError::BadRequest("OAuth login session expired".into()));
    }
    let mut poll_url = validate_oauth_endpoint(&state.poll_url, "poll_url")?;
    poll_url
        .query_pairs_mut()
        .append_pair("uuid", &state.uuid)
        .append_pair("verifier", &state.verifier);
    let response = http
        .get(poll_url)
        .send()
        .await
        .map_err(|error| AppError::Upstream(format!("Cursor OAuth poll failed: {error}")))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(CursorPollResult::Pending {
            retry_after_seconds: 1,
        });
    }
    if !response.status().is_success() {
        return Err(AppError::Upstream(format!(
            "Cursor OAuth poll returned {}",
            response.status()
        )));
    }
    let body = bounded_body(response).await?;
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
    Ok(CursorPollResult::Ready(Box::new(ReadyCursorLogin {
        session_id: state.session_id,
        tenant_external_id: state.tenant_external_id,
        account_name: state.account_name,
        provider_driver: state.provider_driver,
        provider_config: state.provider_config,
        credential: UpstreamCredential::OAuth {
            access_token: tokens.access_token,
            refresh_token: (!tokens.refresh_token.is_empty()).then_some(tokens.refresh_token),
            expires_at: Some(expires_at),
            header: "authorization".to_owned(),
            prefix: "Bearer ".to_owned(),
        },
    })))
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
        .map_err(|error| AppError::Upstream(format!("Cursor OAuth refresh failed: {error}")))?;
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
    })
}

pub(crate) fn validate_oauth_endpoint(value: &str, field: &str) -> Result<Url, AppError> {
    let url = Url::parse(value)
        .map_err(|_| AppError::BadRequest(format!("OAuth {field} must be a URL")))?;
    let private_http = url.scheme() == "http" && url.host_str().is_some_and(is_private_oauth_host);
    if url.scheme() != "https" && !private_http {
        return Err(AppError::BadRequest(format!(
            "OAuth {field} must use HTTPS (private cluster HTTP is allowed)"
        )));
    }
    if url.username() != "" || url.password().is_some() {
        return Err(AppError::BadRequest(format!(
            "OAuth {field} cannot contain credentials"
        )));
    }
    Ok(url)
}

fn is_private_oauth_host(host: &str) -> bool {
    if let Ok(address) = host.parse::<std::net::IpAddr>() {
        return match address {
            std::net::IpAddr::V4(address) => {
                address.is_private() || address.is_loopback() || address.is_link_local()
            }
            std::net::IpAddr::V6(address) => {
                address.is_loopback()
                    || address.is_unique_local()
                    || address.is_unicast_link_local()
            }
        };
    }
    !host.contains('.') || host.ends_with(".svc") || host.ends_with(".svc.cluster.local")
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
        let chunk = chunk.map_err(|error| AppError::Upstream(error.to_string()))?;
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

    #[test]
    fn cursor_login_state_is_encrypted_and_contains_pkce_query() {
        let started = start_cursor_login(
            StartCursorLogin {
                tenant_external_id: "default".to_owned(),
                account_name: "cursor-one".to_owned(),
                provider_driver: "http-json".to_owned(),
                provider_config: json!({"base_url": "https://provider.example"}),
                endpoints: CursorOAuthEndpoints::default(),
                oauth_driver: "cursor".to_owned(),
            },
            b"test material with at least 32 bytes",
            1_000,
        )
        .unwrap();
        assert!(started.login_url.contains("challenge="));
        assert!(started.login_url.contains("uuid="));
        assert!(!started.session_token.contains("cursor-one"));
        assert_eq!(started.expires_at, 601_000);
    }

    #[test]
    fn provider_adapter_allows_private_cluster_http_but_rejects_public_http() {
        let private = start_cursor_login(
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
            },
            b"test material with at least 32 bytes",
            1_000,
        )
        .unwrap();
        assert_eq!(private.driver, "provider_adapter");
        assert!(validate_oauth_endpoint("http://oauth.example.com/login", "login_url").is_err());
    }
}
