use base64::{
    Engine as _,
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
};
use getrandom::fill;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    db::{BeginOAuthLoginSession, Database, OAuthLoginClaim, OAuthLoginSessionReference},
    error::AppError,
    network::{self, OutboundScope},
    provider::{UpstreamCredential, open_private_json, seal_private_json, validate_config},
};

use super::{
    bounded_body,
    endpoint::{
        oauth_adapter_endpoint_scope, validate_oauth_endpoint, validate_oauth_endpoint_with_scope,
    },
};

const CURSOR_SESSION_AAD: &[u8] = b"memeloop-token-center/cursor-oauth-session/v2";
const CURSOR_STATE_AAD: &[u8] = b"memeloop-token-center/cursor-oauth-state/v2";
const CURSOR_READY_AAD: &[u8] = b"memeloop-token-center/cursor-oauth-ready/v2";
const CURSOR_ADAPTER_SCHEMA: &str = "cursor-oauth-v1";
const MAX_CURSOR_IDENTITY_BYTES: usize = 512;
const MAX_CURSOR_TOKEN_BYTES: usize = 128 * 1024;
const MAX_CURSOR_JWT_PAYLOAD_BYTES: usize = 16 * 1024;

pub const DEFAULT_CURSOR_LOGIN_URL: &str = "https://cursor.com/loginDeepControl";
pub const DEFAULT_CURSOR_POLL_URL: &str = "https://api2.cursor.sh/auth/poll";
pub const DEFAULT_CURSOR_REFRESH_URL: &str = "https://api2.cursor.sh/auth/exchange_user_api_key";

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorTokens {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    account: Option<CursorIdentityObject>,
    #[serde(default)]
    user: Option<CursorIdentityObject>,
}

#[derive(Clone, Debug, Deserialize)]
struct CursorIdentityObject {
    id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CursorAdapterState {
    schema: String,
    account_id: String,
}

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
    flow_kind: String,
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
    let provider_scope = network::scope_from_config(&input.provider_config);
    let (mut login_url, poll_url, refresh_url) = if oauth_driver == "provider_adapter" {
        (
            oauth_adapter_endpoint_scope(
                &input.endpoints.login_url,
                "login_url",
                false,
                provider_scope,
            )?
            .0,
            oauth_adapter_endpoint_scope(
                &input.endpoints.poll_url,
                "poll_url",
                false,
                provider_scope,
            )?
            .0,
            oauth_adapter_endpoint_scope(
                &input.endpoints.refresh_url,
                "refresh_url",
                false,
                provider_scope,
            )?
            .0,
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
        flow_kind: if oauth_driver == "cursor" {
            "cursor_pkce"
        } else {
            "provider_adapter_cursor_pkce"
        }
        .to_owned(),
        tenant_external_id: state.tenant_external_id.clone(),
        operator_service_id,
        expires_at,
    };
    db.begin_oauth_login_session(BeginOAuthLoginSession {
        session_id,
        flow_kind: if oauth_driver == "cursor" {
            "cursor_pkce"
        } else {
            "provider_adapter_cursor_pkce"
        }
        .to_owned(),
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

// This is the transport/database boundary for one poll operation. Keeping the
// authorization context explicit makes every caller provide tenant, operator,
// clock, and loopback policy instead of hiding authority in shared state.
#[derive(Clone, Copy)]
pub struct CursorPollAuthority<'a> {
    pub required_tenant: Option<&'a str>,
    pub operator_service_id: Option<Uuid>,
    pub allow_test_loopback: bool,
}

pub async fn poll_cursor_login(
    db: &Database,
    http: &reqwest::Client,
    session_token: &str,
    key_material: &[u8],
    now: i64,
    authority: CursorPollAuthority<'_>,
) -> Result<CursorPollResult, AppError> {
    let session: CursorLoginSessionToken =
        open_private_json(session_token, key_material, CURSOR_SESSION_AAD)
            .map_err(|_| AppError::BadRequest("invalid OAuth session token".into()))?;
    if authority
        .required_tenant
        .is_some_and(|tenant| tenant != session.tenant_external_id)
        || session.operator_service_id != authority.operator_service_id
    {
        return Err(AppError::Forbidden);
    }
    if session.expires_at <= now {
        return Err(AppError::BadRequest("OAuth login session expired".into()));
    }
    let reference = OAuthLoginSessionReference {
        session_id: session.session_id,
        flow_kind: session.flow_kind.clone(),
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
        oauth_adapter_endpoint_scope(
            &state.poll_url,
            "poll_url",
            authority.allow_test_loopback,
            network::scope_from_config(&state.provider_config),
        )?
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
    let outbound_http = network::client_for_url(
        http,
        poll_url.as_str(),
        scope,
        authority.allow_test_loopback,
    )
    .await?;
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
    let tokens: CursorTokens = serde_json::from_slice(&body)
        .map_err(|_| AppError::Upstream("Cursor OAuth returned invalid JSON".into()))?;
    if tokens.access_token.is_empty() {
        return Err(AppError::Upstream(
            "Cursor OAuth returned an empty access token".into(),
        ));
    }
    let expires_at = token_expiry_millis(&tokens.access_token)
        .unwrap_or_else(|| now.saturating_add(60 * 60 * 1000));
    let adapter_state = if state.oauth_driver == "cursor" {
        let account_id = match required_stable_account_id(&tokens) {
            Ok(account_id) => account_id,
            Err(_) => {
                let _ = db
                    .release_oauth_login_poll(state.session_id, lease_owner, now)
                    .await;
                return Err(cursor_identity_upstream_error());
            }
        };
        Some(
            serde_json::to_value(CursorAdapterState {
                schema: CURSOR_ADAPTER_SCHEMA.to_owned(),
                account_id,
            })
            .map_err(|_| AppError::Internal)?,
        )
    } else {
        None
    };
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
            refresh_token: tokens.refresh_token.filter(|value| !value.is_empty()),
            expires_at: Some(expires_at),
            header: "authorization".to_owned(),
            prefix: "Bearer ".to_owned(),
            adapter_state,
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
    configured_scope: OutboundScope,
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
    let refresh_url =
        validate_oauth_endpoint_with_scope(refresh_url, "refresh_url", configured_scope)?;
    let response = http
        .post(refresh_url.as_str())
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
    let tokens: CursorTokens = serde_json::from_slice(&body)
        .map_err(|_| AppError::Upstream("Cursor OAuth refresh returned invalid JSON".into()))?;
    if tokens.access_token.is_empty() {
        return Err(AppError::Upstream(
            "Cursor OAuth refresh returned an empty access token".into(),
        ));
    }
    let expires_at = token_expiry_millis(&tokens.access_token)
        .unwrap_or_else(|| now.saturating_add(60 * 60 * 1000));
    let manages_cursor_identity =
        has_cursor_adapter_state(credential) || refresh_url.as_str() == DEFAULT_CURSOR_REFRESH_URL;
    let current_account_id = if has_cursor_adapter_state(credential) {
        Some(cursor_account_id(credential)?)
    } else {
        None
    };
    let refreshed_account_id = if manages_cursor_identity {
        stable_account_id_from_tokens(&tokens).map_err(|_| cursor_identity_upstream_error())?
    } else {
        None
    };
    if manages_cursor_identity && current_account_id.is_none() && refreshed_account_id.is_none() {
        return Err(cursor_identity_upstream_error());
    }
    if current_account_id
        .as_ref()
        .zip(refreshed_account_id.as_ref())
        .is_some_and(|(current, refreshed)| current != refreshed)
    {
        return Err(AppError::Conflict(
            "Cursor OAuth refresh changed account identity".into(),
        ));
    }
    let adapter_state = match current_account_id.or(refreshed_account_id) {
        Some(account_id) => Some(
            serde_json::to_value(CursorAdapterState {
                schema: CURSOR_ADAPTER_SCHEMA.to_owned(),
                account_id,
            })
            .map_err(|_| AppError::Internal)?,
        ),
        None => credential.adapter_state().cloned(),
    };
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
        adapter_state,
    })
}

pub fn cursor_account_id(credential: &UpstreamCredential) -> Result<String, AppError> {
    let UpstreamCredential::OAuth {
        adapter_state: Some(value),
        ..
    } = credential
    else {
        return Err(cursor_identity_error());
    };
    let state: CursorAdapterState =
        serde_json::from_value(value.clone()).map_err(|_| cursor_identity_error())?;
    if state.schema != CURSOR_ADAPTER_SCHEMA || !valid_stable_identity(&state.account_id) {
        return Err(cursor_identity_error());
    }
    Ok(state.account_id)
}

fn stable_account_id_from_tokens(tokens: &CursorTokens) -> Result<Option<String>, AppError> {
    let explicit = tokens
        .account_id
        .as_deref()
        .or_else(|| tokens.account.as_ref().map(|account| account.id.as_str()))
        .or(tokens.user_id.as_deref())
        .or_else(|| tokens.user.as_ref().map(|user| user.id.as_str()));
    if let Some(explicit) = explicit {
        if !valid_stable_identity(explicit) {
            return Err(cursor_identity_upstream_error());
        }
        return Ok(Some(explicit.to_owned()));
    }
    Ok(jwt_payload(&tokens.access_token)
        .and_then(|value| value.get("sub").and_then(Value::as_str).map(str::to_owned))
        .filter(|subject| valid_stable_identity(subject)))
}

fn required_stable_account_id(tokens: &CursorTokens) -> Result<String, AppError> {
    stable_account_id_from_tokens(tokens)?.ok_or_else(cursor_identity_upstream_error)
}

fn has_cursor_adapter_state(credential: &UpstreamCredential) -> bool {
    credential
        .adapter_state()
        .and_then(|state| state.get("schema"))
        .and_then(Value::as_str)
        == Some(CURSOR_ADAPTER_SCHEMA)
}

fn valid_stable_identity(value: &str) -> bool {
    (1..=MAX_CURSOR_IDENTITY_BYTES).contains(&value.len())
        && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

fn jwt_payload(token: &str) -> Option<Value> {
    if token.len() > MAX_CURSOR_TOKEN_BYTES {
        return None;
    }
    let mut segments = token.split('.');
    let header = segments.next()?;
    let payload = segments.next()?;
    let signature = segments.next()?;
    if header.is_empty() || payload.is_empty() || signature.is_empty() || segments.next().is_some()
    {
        return None;
    }
    let header = decode_base64url(header)?;
    if header.len() > MAX_CURSOR_JWT_PAYLOAD_BYTES {
        return None;
    }
    let header: Value = serde_json::from_slice(&header).ok()?;
    let algorithm = header.get("alg")?.as_str()?;
    if algorithm.is_empty() || algorithm.eq_ignore_ascii_case("none") {
        return None;
    }
    let payload = decode_base64url(payload)?;
    if payload.len() > MAX_CURSOR_JWT_PAYLOAD_BYTES {
        return None;
    }
    serde_json::from_slice(&payload).ok()
}

fn decode_base64url(value: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(value)
        .or_else(|_| URL_SAFE.decode(value))
        .ok()
}

fn token_expiry_millis(token: &str) -> Option<i64> {
    jwt_payload(token)?.get("exp")?.as_i64()?.checked_mul(1_000)
}

fn cursor_identity_error() -> AppError {
    AppError::BadRequest("Cursor OAuth credential has no stable account identity".into())
}

fn cursor_identity_upstream_error() -> AppError {
    AppError::Upstream("Cursor OAuth did not return a stable account identity".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jwt(payload: Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        format!("{header}.{payload}.trusted-endpoint-signature")
    }

    fn tokens(access_token: String) -> CursorTokens {
        CursorTokens {
            access_token,
            refresh_token: Some("refresh-secret".into()),
            account_id: None,
            user_id: None,
            account: None,
            user: None,
        }
    }

    fn credential(account_id: &str, access_token: &str) -> UpstreamCredential {
        UpstreamCredential::OAuth {
            access_token: access_token.into(),
            refresh_token: Some("refresh-secret".into()),
            expires_at: Some(4_000_000_000_000),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: Some(json!({
                "schema": CURSOR_ADAPTER_SCHEMA,
                "account_id": account_id,
            })),
        }
    }

    #[test]
    fn official_identity_wins_and_same_account_is_stable() {
        let mut response = tokens(jwt(json!({
            "sub": "jwt-subject-must-not-override-official-id",
            "name": "untrusted-display-name"
        })));
        response.account_id = Some("cursor-account-123".into());
        assert_eq!(
            required_stable_account_id(&response).unwrap(),
            "cursor-account-123"
        );

        let first = credential("cursor-account-123", "first-access-secret");
        let second = credential("cursor-account-123", "second-access-secret");
        assert_eq!(
            cursor_account_id(&first).unwrap(),
            cursor_account_id(&second).unwrap()
        );
        assert_ne!(
            cursor_account_id(&first).unwrap(),
            cursor_account_id(&credential("cursor-account-456", "third-secret")).unwrap()
        );
    }

    #[test]
    fn jwt_subject_is_the_only_token_payload_identity_fallback() {
        let response = tokens(jwt(json!({
            "sub": "auth0|cursor-stable-subject",
            "name": "changeable display name",
            "email": "changeable@example.test",
            "exp": 4_000_000_000_i64
        })));
        assert_eq!(
            required_stable_account_id(&response).unwrap(),
            "auth0|cursor-stable-subject"
        );
        assert_eq!(
            token_expiry_millis(&response.access_token),
            Some(4_000_000_000_000)
        );
    }

    #[test]
    fn missing_stable_identity_fails_closed_without_leaking_secrets() {
        let opaque_secret = "opaque-access-secret-without-identity";
        let response: CursorTokens = serde_json::from_value(json!({
            "accessToken": opaque_secret,
            "refreshToken": "refresh-secret-that-must-not-leak",
            "name": "arbitrary-display-name-secret",
            "email": "display-secret@example.test"
        }))
        .unwrap();
        let error = required_stable_account_id(&response).unwrap_err();
        let rendered = error.to_string();
        assert_eq!(
            rendered,
            "configured upstream is unavailable: Cursor OAuth did not return a stable account identity"
        );
        for secret in [
            opaque_secret,
            "refresh-secret-that-must-not-leak",
            "arbitrary-display-name-secret",
            "display-secret@example.test",
        ] {
            assert!(!rendered.contains(secret));
        }

        let no_subject = tokens(jwt(json!({
            "name": "jwt-display-secret",
            "email": "jwt-secret@example.test"
        })));
        assert!(required_stable_account_id(&no_subject).is_err());
        let unsigned_header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(br#"{"sub":"forged-subject"}"#);
        assert!(
            required_stable_account_id(&tokens(format!("{unsigned_header}.{payload}.forged")))
                .is_err()
        );
    }

    #[test]
    fn credential_identity_errors_and_debug_are_redacted() {
        let malformed = UpstreamCredential::OAuth {
            access_token: "access-secret".into(),
            refresh_token: Some("refresh-secret".into()),
            expires_at: Some(4_000_000_000_000),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: Some(json!({
                "schema": CURSOR_ADAPTER_SCHEMA,
                "display_name": "identity-display-secret"
            })),
        };
        let rendered = cursor_account_id(&malformed).unwrap_err().to_string();
        assert_eq!(
            rendered,
            "invalid request: Cursor OAuth credential has no stable account identity"
        );
        let debug = format!("{malformed:?}");
        for secret in ["access-secret", "refresh-secret", "identity-display-secret"] {
            assert!(!rendered.contains(secret));
            assert!(!debug.contains(secret));
        }
    }
}
