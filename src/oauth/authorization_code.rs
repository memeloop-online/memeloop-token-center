//! Host-owned standard authorization-code + S256 flow for provider contributions.
//! Plugins declare endpoints; the host owns state, credentials and proxy transport.
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use url::Url;
use uuid::Uuid;

use super::OAuthRefreshRequestGuard;
use crate::{
    db::{BeginOAuthLoginSession, Database, OAuthLoginClaim, OAuthLoginSessionReference},
    error::AppError,
    network::{self, OutboundScope},
    provider::{
        OAuthAdapterContribution, OAuthFlowKind, UpstreamCredential, open_private_json,
        seal_private_json,
    },
};

pub const FLOW: &str = "generic_authorization_code";
const TOKEN_AAD: &[u8] = b"memeloop-token-center/authorization-code/session/v1";
const STATE_AAD: &[u8] = b"memeloop-token-center/authorization-code/state/v1";
const READY_AAD: &[u8] = b"memeloop-token-center/authorization-code/ready/v1";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    pub client_id: String,
    pub redirect_uri: String,
    pub scopes: Vec<String>,
    /// Only needed by clients whose token endpoint requires this parameter.
    /// This struct belongs in encrypted session/credential state, never public config.
    pub client_secret: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct StartInput {
    #[serde(default)]
    pub application_plugin_revision: Option<i64>,
    pub tenant_external_id: String,
    pub account_name: String,
    pub provider_driver: String,
    pub provider_config: Value,
    pub operator_service_id: Option<Uuid>,
    pub client: ClientConfig,
    pub adapter: OAuthAdapterContribution,
    pub proxy_url: Option<String>,
    pub proxy_network_scope: Option<OutboundScope>,
}

#[derive(Serialize)]
pub struct LoginStart {
    pub driver: String,
    pub login_url: String,
    pub session_token: String,
    pub expires_at: i64,
}

#[derive(Clone, Serialize, Deserialize)]
struct Session {
    #[serde(default)]
    application_plugin_revision: Option<i64>,
    session_id: Uuid,
    tenant_external_id: String,
    operator_service_id: Option<Uuid>,
    expires_at: i64,
}

#[derive(Serialize, Deserialize)]
struct LoginState {
    input: StartInput,
    state: String,
    verifier: String,
}

#[derive(Serialize, Deserialize)]
pub struct ReadyLogin {
    #[serde(default)]
    pub application_plugin_revision: Option<i64>,
    pub session_id: Uuid,
    pub tenant_external_id: String,
    pub account_name: String,
    pub provider_driver: String,
    pub provider_config: Value,
    pub refresh_url: String,
    pub credential: UpstreamCredential,
}

pub enum CompleteResult {
    Pending {
        retry_after_seconds: u64,
    },
    Consumed {
        account_id: Uuid,
        tenant_external_id: String,
    },
    Ready {
        lease_owner: Uuid,
        login: Box<ReadyLogin>,
    },
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
    #[serde(default)]
    token_type: Option<String>,
}

pub async fn start(
    db: &Database,
    input: StartInput,
    key: &[u8],
    now: i64,
) -> Result<LoginStart, AppError> {
    validate_client(&input.client)?;
    if input.adapter.flow_kind != OAuthFlowKind::AuthorizationCodePkce
        || input.adapter.api_version != "oauth-adapter-v1"
    {
        return Err(AppError::BadRequest(
            "provider does not declare standard authorization-code OAuth".into(),
        ));
    }
    for endpoint in [
        &input.adapter.login_url,
        &input.adapter.poll_url,
        &input.adapter.refresh_url,
    ] {
        super::validate_oauth_adapter_endpoint(endpoint, "OAuth endpoint")?;
    }
    if input.account_name.trim().is_empty() || input.account_name.len() > 200 {
        return Err(AppError::BadRequest("account name is required".into()));
    }
    let mut random = [0u8; 64];
    getrandom::fill(&mut random).map_err(|_| AppError::Internal)?;
    let verifier = URL_SAFE_NO_PAD.encode(&random[..32]);
    let state = URL_SAFE_NO_PAD.encode(&random[32..]);
    let mut url = Url::parse(&input.adapter.login_url).map_err(|_| AppError::Internal)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &input.client.client_id)
        .append_pair("redirect_uri", &input.client.redirect_uri)
        .append_pair("scope", &input.client.scopes.join(" "))
        .append_pair("state", &state)
        .append_pair(
            "code_challenge",
            &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        )
        .append_pair("code_challenge_method", "S256")
        .append_pair("access_type", "offline");
    let session = Session {
        application_plugin_revision: input.application_plugin_revision,
        session_id: Uuid::now_v7(),
        tenant_external_id: input.tenant_external_id.clone(),
        operator_service_id: input.operator_service_id,
        expires_at: now.saturating_add(600_000),
    };
    let driver = input.provider_driver.clone();
    db.begin_oauth_login_session(BeginOAuthLoginSession {
        session_id: session.session_id,
        flow_kind: FLOW.into(),
        tenant_external_id: session.tenant_external_id.clone(),
        operator_service_id: session.operator_service_id,
        state_ciphertext: seal_private_json(
            &LoginState {
                input,
                state,
                verifier,
            },
            key,
            STATE_AAD,
        )?,
        next_poll_at: now,
        expires_at: session.expires_at,
    })
    .await?;
    Ok(LoginStart {
        driver,
        login_url: url.into(),
        session_token: seal_private_json(&session, key, TOKEN_AAD)?,
        expires_at: session.expires_at,
    })
}

#[allow(clippy::too_many_arguments)]
pub async fn complete(
    db: &Database,
    http: &reqwest::Client,
    token: &str,
    callback_url: &str,
    required_tenant: Option<&str>,
    operator_service_id: Option<Uuid>,
    key: &[u8],
    now: i64,
    allow_test_loopback: bool,
) -> Result<CompleteResult, AppError> {
    let session: Session = open_private_json(token, key, TOKEN_AAD)
        .map_err(|_| AppError::BadRequest("invalid OAuth session".into()))?;
    if required_tenant.is_some_and(|tenant| tenant != session.tenant_external_id)
        || session.operator_service_id != operator_service_id
    {
        return Err(AppError::Forbidden);
    }
    if session.expires_at <= now {
        return Err(AppError::BadRequest("OAuth session expired".into()));
    }
    let reference = OAuthLoginSessionReference {
        session_id: session.session_id,
        flow_kind: FLOW.into(),
        tenant_external_id: session.tenant_external_id.clone(),
        operator_service_id,
        expires_at: session.expires_at,
    };
    let (lease_owner, ciphertext) = match db.claim_oauth_login_poll(&reference, now, 1).await? {
        OAuthLoginClaim::Pending {
            retry_after_seconds,
        } => {
            return Ok(CompleteResult::Pending {
                retry_after_seconds,
            });
        }
        OAuthLoginClaim::Consumed { account_id } => {
            return Ok(CompleteResult::Consumed {
                account_id,
                tenant_external_id: session.tenant_external_id,
            });
        }
        OAuthLoginClaim::Ready {
            lease_owner,
            ready_ciphertext,
        } => {
            return Ok(CompleteResult::Ready {
                lease_owner,
                login: Box::new(open_private_json(&ready_ciphertext, key, READY_AAD)?),
            });
        }
        OAuthLoginClaim::Claimed {
            lease_owner,
            state_ciphertext,
        } => (lease_owner, state_ciphertext),
    };
    let login: LoginState = open_private_json(&ciphertext, key, STATE_AAD)?;
    if login.input.application_plugin_revision != session.application_plugin_revision {
        return Err(AppError::Conflict("OAuth application revision binding changed".into()));
    }
    let code = match callback_code(callback_url, &login.input.client.redirect_uri, &login.state) {
        Ok(code) => code,
        Err(error) => {
            db.release_oauth_login_poll(session.session_id, lease_owner, now)
                .await?;
            return Err(error);
        }
    };
    let mut form = vec![
        ("grant_type", "authorization_code".to_owned()),
        ("code", code),
        ("redirect_uri", login.input.client.redirect_uri.clone()),
        ("code_verifier", login.verifier.clone()),
    ];
    add_client(&mut form, &login.input.client);
    let tokens = token_request(
        http,
        &login.input.adapter.poll_url,
        &login.input.provider_config,
        login
            .input
            .proxy_url
            .as_deref()
            .zip(login.input.proxy_network_scope),
        &form,
        allow_test_loopback,
        None,
    )
    .await.and_then(|tokens| credential(tokens, &login.input, now, None));
    let credential = match tokens {
        Ok(credential) => credential,
        Err(error) => {
            db.fail_oauth_login_poll(session.session_id, lease_owner, now).await?;
            return Err(error);
        }
    };
    let ready = ReadyLogin {
        application_plugin_revision: session.application_plugin_revision,
        session_id: session.session_id,
        tenant_external_id: login.input.tenant_external_id,
        account_name: login.input.account_name,
        provider_driver: login.input.provider_driver,
        provider_config: login.input.provider_config,
        refresh_url: login.input.adapter.refresh_url,
        credential,
    };
    db.stage_oauth_login_ready(
        session.session_id,
        lease_owner,
        seal_private_json(&ready, key, READY_AAD)?,
        now,
    )
    .await?;
    match db.claim_oauth_login_poll(&reference, now, 1).await? {
        OAuthLoginClaim::Ready {
            lease_owner,
            ready_ciphertext,
        } => Ok(CompleteResult::Ready {
            lease_owner,
            login: Box::new(open_private_json(&ready_ciphertext, key, READY_AAD)?),
        }),
        OAuthLoginClaim::Consumed { account_id } => Ok(CompleteResult::Consumed {
            account_id,
            tenant_external_id: session.tenant_external_id,
        }),
        OAuthLoginClaim::Pending {
            retry_after_seconds,
        } => Ok(CompleteResult::Pending {
            retry_after_seconds,
        }),
        OAuthLoginClaim::Claimed { .. } => Err(AppError::Internal),
    }
}

pub async fn refresh(
    http: &reqwest::Client,
    current: &UpstreamCredential,
    active_adapter: &OAuthAdapterContribution,
    now: i64,
    allow_test_loopback: bool,
    guard: &dyn OAuthRefreshRequestGuard,
) -> Result<UpstreamCredential, AppError> {
    let UpstreamCredential::OAuth {
        refresh_token: Some(refresh),
        adapter_state: Some(state),
        ..
    } = current
    else {
        return Err(AppError::BadRequest(
            "OAuth refresh state is missing".into(),
        ));
    };
    let input: StartInput = serde_json::from_value(
        state
            .get("authorization_code")
            .cloned()
            .ok_or(AppError::Internal)?,
    )
    .map_err(|_| AppError::BadRequest("invalid OAuth refresh state".into()))?;
    if active_adapter.flow_kind != OAuthFlowKind::AuthorizationCodePkce
        || active_adapter.refresh_url != input.adapter.refresh_url
    {
        return Err(AppError::Conflict(
            "OAuth contribution changed; reconnect this account".into(),
        ));
    }
    let mut form = vec![
        ("grant_type", "refresh_token".to_owned()),
        ("refresh_token", refresh.clone()),
    ];
    add_client(&mut form, &input.client);
    let tokens = token_request(
        http,
        &active_adapter.refresh_url,
        &input.provider_config,
        current.proxy(),
        &form,
        allow_test_loopback,
        Some(guard),
    )
    .await?;
    Ok(credential(tokens, &input, now, Some(refresh))?.preserve_proxy_from(current))
}

fn credential(
    tokens: TokenResponse,
    input: &StartInput,
    now: i64,
    old_refresh: Option<&String>,
) -> Result<UpstreamCredential, AppError> {
    if tokens.access_token.is_empty()
        || tokens.access_token.len() > 128 * 1024
        || tokens.expires_in <= 0
        || tokens.expires_in > 31_536_000
        || tokens
            .token_type
            .as_deref()
            .is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer"))
    {
        return Err(AppError::Upstream("invalid OAuth token response".into()));
    }
    Ok(UpstreamCredential::OAuth {
        access_token: tokens.access_token,
        refresh_token: tokens
            .refresh_token
            .filter(|token| !token.is_empty())
            .or_else(|| old_refresh.cloned()),
        expires_at: Some(
            now.checked_add(tokens.expires_in * 1000)
                .ok_or(AppError::Internal)?,
        ),
        header: "authorization".into(),
        prefix: "Bearer ".into(),
        adapter_state: Some(json!({"authorization_code": input})),
        proxy_url: input.proxy_url.clone(),
        proxy_network_scope: input.proxy_network_scope,
    })
}

async fn token_request(
    http: &reqwest::Client,
    endpoint: &str,
    config: &Value,
    proxy: Option<(&str, OutboundScope)>,
    form: &[(&str, String)],
    allow_test_loopback: bool,
    guard: Option<&dyn OAuthRefreshRequestGuard>,
) -> Result<TokenResponse, AppError> {
    let http =
        network::client_for_config_url_no_retry(http, endpoint, config, proxy, allow_test_loopback).await?;
    let request = http
        .post(endpoint)
        .timeout(std::time::Duration::from_secs(20))
        .form(form);
    if let Some(guard) = guard {
        guard.mark_request_started().await?;
    }
    let response = request
        .send()
        .await
        .map_err(|_| AppError::Upstream("OAuth token request failed".into()))?;
    let status = response.status();
    let body = super::bounded_body(response).await?;
    if !status.is_success() {
        return Err(AppError::Upstream(format!(
            "OAuth token endpoint returned HTTP {}",
            status.as_u16()
        )));
    }
    serde_json::from_slice(&body)
        .map_err(|_| AppError::Upstream("invalid OAuth token response".into()))
}

fn add_client(form: &mut Vec<(&'static str, String)>, client: &ClientConfig) {
    form.push(("client_id", client.client_id.clone()));
    if let Some(secret) = &client.client_secret {
        form.push(("client_secret", secret.clone()));
    }
}

fn validate_client(client: &ClientConfig) -> Result<(), AppError> {
    let redirect = Url::parse(&client.redirect_uri)
        .map_err(|_| AppError::BadRequest("invalid OAuth redirect URI".into()))?;
    let loopback = redirect
        .host_str()
        .is_some_and(|host| matches!(host, "127.0.0.1" | "[::1]"));
    if client.client_id.is_empty()
        || client.client_id.len() > 1024
        || client.scopes.is_empty()
        || client.scopes.len() > 32
        || client.scopes.iter().any(|scope| {
            scope.is_empty() || scope.len() > 512 || scope.chars().any(char::is_whitespace)
        })
        || (redirect.scheme() != "https" && !(redirect.scheme() == "http" && loopback))
        || redirect.query().is_some()
        || redirect.fragment().is_some()
        || !redirect.username().is_empty()
        || redirect.password().is_some()
    {
        return Err(AppError::BadRequest(
            "invalid OAuth client configuration".into(),
        ));
    }
    Ok(())
}

fn callback_code(callback: &str, redirect: &str, state: &str) -> Result<String, AppError> {
    let mut callback = Url::parse(callback)
        .map_err(|_| AppError::BadRequest("paste the complete OAuth callback URL".into()))?;
    let pairs: Vec<_> = callback
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    callback.set_query(None);
    if callback.as_str() != redirect || callback.fragment().is_some() {
        return Err(AppError::BadRequest(
            "OAuth callback destination did not match".into(),
        ));
    }
    let values = |key: &str| {
        pairs
            .iter()
            .filter(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
            .collect::<Vec<_>>()
    };
    let states = values("state");
    let codes = values("code");
    if states.len() != 1
        || states[0].as_bytes().ct_eq(state.as_bytes()).unwrap_u8() != 1
        || codes.len() != 1
        || codes[0].is_empty()
        || codes[0].len() > 8192
        || !values("error").is_empty()
    {
        return Err(AppError::BadRequest(
            "OAuth callback code/state is invalid".into(),
        ));
    }
    Ok(codes[0].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn callback_binds_destination_and_unique_state() {
        assert_eq!(
            callback_code(
                "http://127.0.0.1:51121/oauth-callback?code=fixture&state=state",
                "http://127.0.0.1:51121/oauth-callback",
                "state"
            )
            .unwrap(),
            "fixture"
        );
        for callback in [
            "https://other.example/callback?code=x&state=state",
            "http://127.0.0.1:51121/oauth-callback?code=x&state=state&state=state",
            "http://127.0.0.1:51121/oauth-callback?code=x&state=wrong",
        ] {
            assert!(
                callback_code(callback, "http://127.0.0.1:51121/oauth-callback", "state").is_err()
            );
        }
    }
}
