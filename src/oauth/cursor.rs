use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
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
        oauth_adapter_endpoint_scope(
            &state.poll_url,
            "poll_url",
            allow_test_loopback,
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

fn token_expiry_millis(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let payload = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value: Value = serde_json::from_slice(&payload).ok()?;
    value.get("exp")?.as_i64()?.checked_mul(1_000)
}
