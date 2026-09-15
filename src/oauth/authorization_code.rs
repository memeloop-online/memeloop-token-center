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
pub const CLIENT_DEFAULTS_ENV: &str = "MTC_PROVIDER_OAUTH_CLIENT_DEFAULTS_JSON";
const READY_RECOVERY_MILLIS: i64 = 24 * 60 * 60 * 1000;

/// Deployment-owned Secret injection. Values never appear in config debug or
/// public catalog JSON; the chosen client is snapshotted into the AEAD session.
pub fn deployment_client_default(provider: &str) -> Result<ClientConfig, AppError> {
    let raw = std::env::var(CLIENT_DEFAULTS_ENV).map_err(|_| {
        AppError::Conflict("default OAuth client configuration is not provisioned".into())
    })?;
    if raw.len() > 64 * 1024 {
        return Err(AppError::Conflict(
            "default OAuth client configuration exceeds limits".into(),
        ));
    }
    let mut clients: std::collections::BTreeMap<String, ClientConfig> = serde_json::from_str(&raw)
        .map_err(|_| AppError::Conflict("default OAuth client configuration is invalid".into()))?;
    let client = clients.remove(provider).ok_or_else(|| {
        AppError::Conflict(
            "default OAuth client configuration is not provisioned for this provider".into(),
        )
    })?;
    validate_client(&client)?;
    Ok(client)
}
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshState {
    client_id: String,
    client_secret: Option<String>,
    refresh_url: String,
    network_scope: OutboundScope,
    #[serde(default)]
    login_client: Option<ClientConfig>,
}

fn refresh_state(input: &StartInput) -> Result<Value, AppError> {
    let state = json!({"authorization_code": RefreshState {
        client_id: input.client.client_id.clone(),
        client_secret: input.client.client_secret.clone(),
        refresh_url: input.adapter.refresh_url.clone(),
        network_scope: network::scope_from_config(&input.provider_config),
        login_client: Some(input.client.clone()),
    }});
    crate::provider::validate_adapter_state(&state)?;
    Ok(state)
}

/// Reuse the installed OAuth client, not a newly selected client for the same
/// provider. Older credentials predate persisted callback/scopes; their explicit
/// operator choice or deployment default must still match the installed client.
pub(crate) fn reauthorization_client(
    credential: &UpstreamCredential,
    requested: Option<ClientConfig>,
    provider_driver: &str,
    refresh_url: &str,
) -> Result<ClientConfig, AppError> {
    let state: RefreshState = serde_json::from_value(
        credential
            .adapter_state()
            .and_then(|value| value.get("authorization_code"))
            .cloned()
            .ok_or_else(|| {
                AppError::Conflict("installed OAuth client metadata is unavailable".into())
            })?,
    )
    .map_err(|_| AppError::Conflict("installed OAuth client metadata is invalid".into()))?;
    let client = match (state.login_client, requested) {
        (Some(installed), None) => installed,
        (Some(installed), Some(requested)) => {
            if serde_json::to_value(&installed).map_err(|_| AppError::Internal)?
                != serde_json::to_value(&requested).map_err(|_| AppError::Internal)?
            {
                return Err(AppError::BadRequest(
                    "reauthorization cannot change the installed OAuth client or scopes".into(),
                ));
            }
            installed
        }
        (None, Some(requested)) => requested,
        (None, None) => deployment_client_default(provider_driver)?,
    };
    if client.client_id != state.client_id
        || client.client_secret != state.client_secret
        || refresh_url != state.refresh_url
    {
        return Err(AppError::Conflict(
            "reauthorization must use the installed OAuth client and refresh endpoint".into(),
        ));
    }
    Ok(client)
}

fn validate_start_proxy(input: &StartInput) -> Result<(), AppError> {
    match (input.proxy_url.as_deref(), input.proxy_network_scope) {
        (None, None) => Ok(()),
        (Some(proxy), Some(OutboundScope::Private)) => {
            crate::provider::validate_proxy_url(proxy)?;
            let parsed = Url::parse(proxy)
                .map_err(|_| AppError::BadRequest("invalid OAuth proxy".into()))?;
            if parsed.scheme() == "socks5h" && !network::has_safe_private_ip_literal_host(&parsed) {
                return Err(AppError::BadRequest(
                    "remote-DNS OAuth proxy must use a private IP endpoint".into(),
                ));
            }
            Ok(())
        }
        _ => Err(AppError::BadRequest(
            "OAuth proxy must use private network scope".into(),
        )),
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct StartInput {
    #[serde(default)]
    pub reauthorize: Option<super::OAuthReauthorizationTarget>,
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
    pub recovery_expires_at: i64,
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
    #[serde(default)]
    exchange_started: bool,
    input: StartInput,
    state: String,
    verifier: String,
}

#[derive(Serialize, Deserialize)]
pub struct ReadyLogin {
    #[serde(default)]
    pub reauthorize: Option<super::OAuthReauthorizationTarget>,
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

/// Validate the sealed session's authority before selecting its historical runtime.
pub fn session_application_revision(
    token: &str,
    key: &[u8],
    required_tenant: Option<&str>,
    operator_service_id: Option<Uuid>,
    now: i64,
) -> Result<Option<i64>, AppError> {
    let session: Session = open_private_json(token, key, TOKEN_AAD)
        .map_err(|_| AppError::BadRequest("invalid OAuth session".into()))?;
    if required_tenant.is_some_and(|tenant| tenant != session.tenant_external_id)
        || session.operator_service_id != operator_service_id
    {
        return Err(AppError::Forbidden);
    }
    if session.expires_at.saturating_add(READY_RECOVERY_MILLIS) <= now {
        return Err(AppError::BadRequest("OAuth session expired".into()));
    }
    Ok(session.application_plugin_revision)
}

fn open_ready(
    ciphertext: &str,
    key: &[u8],
    session: &Session,
) -> Result<Box<ReadyLogin>, AppError> {
    let ready: ReadyLogin = open_private_json(ciphertext, key, READY_AAD)?;
    if ready.application_plugin_revision != session.application_plugin_revision
        || ready.session_id != session.session_id
        || ready.tenant_external_id != session.tenant_external_id
    {
        return Err(AppError::Conflict(
            "OAuth ready session binding changed".into(),
        ));
    }
    Ok(Box::new(ready))
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
    mut input: StartInput,
    key: &[u8],
    now: i64,
) -> Result<LoginStart, AppError> {
    validate_client(&input.client)?;
    input.client.redirect_uri = Url::parse(&input.client.redirect_uri)
        .map_err(|_| AppError::BadRequest("invalid OAuth redirect URI".into()))?
        .to_string();
    validate_start_proxy(&input)?;
    let _ = refresh_state(&input)?;
    if input.provider_driver == crate::provider::antigravity::DRIVER {
        let _ = crate::provider::antigravity::Config::from_account(&input.provider_config)?;
    }
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
                exchange_started: false,
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
        recovery_expires_at: session.expires_at.saturating_add(READY_RECOVERY_MILLIS),
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
    if session.expires_at.saturating_add(READY_RECOVERY_MILLIS) <= now {
        return Err(AppError::BadRequest("OAuth session expired".into()));
    }
    let reference = OAuthLoginSessionReference {
        session_id: session.session_id,
        flow_kind: FLOW.into(),
        tenant_external_id: session.tenant_external_id.clone(),
        operator_service_id,
        expires_at: session.expires_at,
    };
    let (lease_owner, ciphertext) =
        match db.claim_oauth_code_ready_recovery(&reference, now).await? {
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
                    login: open_ready(&ready_ciphertext, key, &session)?,
                });
            }
            OAuthLoginClaim::Claimed {
                lease_owner,
                state_ciphertext,
            } => (lease_owner, state_ciphertext),
        };
    let mut login: LoginState = open_private_json(&ciphertext, key, STATE_AAD)?;
    if login.input.application_plugin_revision != session.application_plugin_revision {
        return Err(AppError::Conflict(
            "OAuth application revision binding changed".into(),
        ));
    }
    // Empty callbacks are explicit status/finalization checks from the UI.
    // They may claim a Ready result, but may never arm, exchange or fail a code.
    if callback_url.is_empty() {
        db.release_oauth_login_poll(session.session_id, lease_owner, now)
            .await?;
        return Err(AppError::Conflict(
            "OAuth credentials are not ready for finalization".into(),
        ));
    }
    if login.exchange_started {
        db.fail_oauth_login_poll(session.session_id, lease_owner, now)
            .await?;
        return Err(AppError::Conflict(
            "OAuth code exchange outcome is unknown; start a new login".into(),
        ));
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
    let prepared = match prepare_token_request(
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
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(error) => {
            db.release_oauth_login_poll(session.session_id, lease_owner, now)
                .await?;
            return Err(error);
        }
    };
    // Validate the final encrypted refresh envelope before crossing the send fence.
    if let Err(error) = refresh_state(&login.input) {
        db.release_oauth_login_poll(session.session_id, lease_owner, now)
            .await?;
        return Err(error);
    }
    // A reclaimed lease may not replay an authorization code after a crash.
    // This one-way marker is encrypted with the existing session state and
    // becomes durable before any token request can be dispatched.
    login.exchange_started = true;
    db.replace_oauth_login_poll_state(
        session.session_id,
        lease_owner,
        seal_private_json(&login, key, STATE_AAD)?,
    )
    .await?;
    let tokens = dispatch_token_request(prepared)
        .await
        .and_then(|tokens| credential(tokens, &login.input, now, None));
    let credential = match tokens {
        Ok(credential) => credential,
        Err(error) => {
            db.fail_oauth_login_poll(session.session_id, lease_owner, now)
                .await?;
            return Err(error);
        }
    };
    let ready = ReadyLogin {
        reauthorize: login.input.reauthorize,
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
    match db.claim_oauth_code_ready_recovery(&reference, now).await? {
        OAuthLoginClaim::Ready {
            lease_owner,
            ready_ciphertext,
        } => Ok(CompleteResult::Ready {
            lease_owner,
            login: open_ready(&ready_ciphertext, key, &session)?,
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
    let input: RefreshState = serde_json::from_value(
        state
            .get("authorization_code")
            .cloned()
            .ok_or(AppError::Internal)?,
    )
    .map_err(|_| AppError::BadRequest("invalid OAuth refresh state".into()))?;
    if active_adapter.flow_kind != OAuthFlowKind::AuthorizationCodePkce
        || active_adapter.refresh_url != input.refresh_url
    {
        return Err(AppError::Conflict(
            "OAuth contribution changed; reconnect this account".into(),
        ));
    }
    let mut form = vec![
        ("grant_type", "refresh_token".to_owned()),
        ("refresh_token", refresh.clone()),
    ];
    form.push(("client_id", input.client_id.clone()));
    if let Some(secret) = &input.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    let tokens = token_request(
        http,
        &active_adapter.refresh_url,
        &json!({"network_scope": input.network_scope}),
        current.proxy(),
        &form,
        allow_test_loopback,
        Some(guard),
    )
    .await?;
    Ok(
        credential_with_state(tokens, state.clone(), now, Some(refresh))?
            .preserve_proxy_from(current),
    )
}

fn credential(
    tokens: TokenResponse,
    input: &StartInput,
    now: i64,
    old_refresh: Option<&String>,
) -> Result<UpstreamCredential, AppError> {
    let mut credential = credential_with_state(tokens, refresh_state(input)?, now, old_refresh)?;
    if let UpstreamCredential::OAuth {
        proxy_url,
        proxy_network_scope,
        ..
    } = &mut credential
    {
        *proxy_url = input.proxy_url.clone();
        *proxy_network_scope = input.proxy_network_scope;
    }
    Ok(credential)
}

fn credential_with_state(
    tokens: TokenResponse,
    state: Value,
    now: i64,
    old_refresh: Option<&String>,
) -> Result<UpstreamCredential, AppError> {
    crate::provider::validate_adapter_state(&state)?;
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
        adapter_state: Some(state),
        proxy_url: None,
        proxy_network_scope: None,
    })
}

#[allow(clippy::too_many_arguments)]
async fn token_request(
    http: &reqwest::Client,
    endpoint: &str,
    config: &Value,
    proxy: Option<(&str, OutboundScope)>,
    form: &[(&str, String)],
    allow_test_loopback: bool,
    guard: Option<&dyn OAuthRefreshRequestGuard>,
) -> Result<TokenResponse, AppError> {
    let prepared =
        prepare_token_request(http, endpoint, config, proxy, form, allow_test_loopback).await?;
    if let Some(guard) = guard {
        guard.mark_request_started().await?;
    }
    dispatch_token_request(prepared).await
}

async fn prepare_token_request(
    http: &reqwest::Client,
    endpoint: &str,
    config: &Value,
    proxy: Option<(&str, OutboundScope)>,
    form: &[(&str, String)],
    allow_test_loopback: bool,
) -> Result<(reqwest::Client, reqwest::Request), AppError> {
    let http =
        network::client_for_config_url_no_retry(http, endpoint, config, proxy, allow_test_loopback)
            .await?;
    let request = http
        .post(endpoint)
        .timeout(std::time::Duration::from_secs(20))
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(reqwest::header::ACCEPT, "application/json")
        .body(encode_token_form(form))
        .build()
        .map_err(|_| AppError::BadRequest("OAuth token request is invalid".into()))?;
    Ok((http, request))
}

async fn dispatch_token_request(
    (http, request): (reqwest::Client, reqwest::Request),
) -> Result<TokenResponse, AppError> {
    let response = http
        .execute(request)
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

fn encode_token_form(form: &[(&str, String)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form.iter().map(|(name, value)| (*name, value.as_str())))
        .finish()
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
        .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "[::1]"));
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
    let expected = Url::parse(redirect)
        .map_err(|_| AppError::BadRequest("invalid OAuth redirect URI".into()))?;
    if callback != expected || callback.fragment().is_some() {
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
    fn reauthorization_preserves_installed_client_and_encrypted_target() {
        let mut original = input("https://tokens.example.com/token".into());
        let target = super::super::OAuthReauthorizationTarget {
            account_id: Uuid::now_v7(),
            expected_updated_at: 123,
            expected_credential_generation: 4,
        };
        original.reauthorize = Some(target.clone());
        let issued = credential(
            TokenResponse {
                access_token: "fixture-access".into(),
                refresh_token: Some("fixture-refresh".into()),
                expires_in: 3600,
                token_type: None,
            },
            &original,
            1000,
            None,
        )
        .unwrap();
        let reused = reauthorization_client(
            &issued,
            None,
            "fixture-provider",
            &original.adapter.refresh_url,
        )
        .unwrap();
        assert_eq!(reused.scopes, original.client.scopes);
        assert_eq!(reused.redirect_uri, original.client.redirect_uri);
        let mut changed = original.client.clone();
        changed.scopes.push("unrequested-scope".into());
        assert!(
            reauthorization_client(
                &issued,
                Some(changed),
                "fixture-provider",
                &original.adapter.refresh_url
            )
            .is_err()
        );
        assert!(
            reauthorization_client(
                &issued,
                None,
                "fixture-provider",
                "https://other.example.com/token"
            )
            .is_err()
        );
        let key = b"generic reauthorization fixture encryption key";
        let cipher = seal_private_json(&original, key, STATE_AAD).unwrap();
        let restored: StartInput = open_private_json(&cipher, key, STATE_AAD).unwrap();
        assert_eq!(restored.reauthorize, Some(target));
        let mut legacy = serde_json::to_value(&original).unwrap();
        legacy.as_object_mut().unwrap().remove("reauthorize");
        assert!(
            serde_json::from_value::<StartInput>(legacy)
                .unwrap()
                .reauthorize
                .is_none()
        );
    }
    #[tokio::test]
    async fn issued_ready_result_recovers_after_authorization_expiry_without_reexchange() {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("ready-recovery.db").display()
        ))
        .await
        .unwrap();
        db.migrate().await.unwrap();
        let key = b"fixture encryption key at least 32 bytes long";
        let input = input("https://tokens.example.com/token".into());
        let started = start(&db, input.clone(), key, 1000).await.unwrap();
        let session: Session = open_private_json(&started.session_token, key, TOKEN_AAD).unwrap();
        let reference = OAuthLoginSessionReference {
            session_id: session.session_id,
            flow_kind: FLOW.into(),
            tenant_external_id: session.tenant_external_id.clone(),
            operator_service_id: None,
            expires_at: session.expires_at,
        };
        let OAuthLoginClaim::Claimed { lease_owner, .. } = db
            .claim_oauth_login_poll(&reference, started.expires_at - 1000, 1)
            .await
            .unwrap()
        else {
            panic!("initial lease")
        };
        let issued = credential(
            TokenResponse {
                access_token: "fixture-access".into(),
                refresh_token: Some("fixture-refresh".into()),
                expires_in: 3600,
                token_type: None,
            },
            &input,
            started.expires_at - 500,
            None,
        )
        .unwrap();
        let ready = ReadyLogin {
            reauthorize: None,
            application_plugin_revision: session.application_plugin_revision,
            session_id: session.session_id,
            tenant_external_id: session.tenant_external_id,
            account_name: input.account_name,
            provider_driver: input.provider_driver,
            provider_config: input.provider_config,
            refresh_url: input.adapter.refresh_url,
            credential: issued,
        };
        db.stage_oauth_login_ready(
            session.session_id,
            lease_owner,
            seal_private_json(&ready, key, READY_AAD).unwrap(),
            started.expires_at - 500,
        )
        .await
        .unwrap();
        let now = started.expires_at + 1000;
        let result = complete(
            &db,
            &reqwest::Client::new(),
            &started.session_token,
            "unused-for-ready-recovery",
            None,
            None,
            key,
            now,
            false,
        )
        .await
        .unwrap();
        let CompleteResult::Ready { lease_owner, login } = result else {
            panic!("issued credentials remain recoverable")
        };
        let ready = *login;
        let account = db
            .create_upstream_account(
                crate::db::CreateUpstreamAccountInput {
                    tenant_external_id: ready.tenant_external_id,
                    name: ready.account_name,
                    driver: ready.provider_driver,
                    config: ready.provider_config,
                    credential: ready.credential,
                    oauth_session_id: Some(ready.session_id),
                    oauth_driver: Some(FLOW.into()),
                    oauth_refresh_url: Some(ready.refresh_url),
                },
                key,
            )
            .await
            .unwrap();
        db.finish_oauth_login_session(ready.session_id, lease_owner, account.id, now)
            .await
            .unwrap();
        assert!(matches!(
            complete(
                &db,
                &reqwest::Client::new(),
                &started.session_token,
                "unused-for-consumed-replay",
                None,
                None,
                key,
                now + 1,
                false
            )
            .await
            .unwrap(),
            CompleteResult::Consumed { .. }
        ));
        assert!(
            complete(
                &db,
                &reqwest::Client::new(),
                &started.session_token,
                "unused",
                None,
                None,
                key,
                started.recovery_expires_at,
                false
            )
            .await
            .is_err()
        );
    }
    #[test]
    fn compact_refresh_state_excludes_provider_headers_and_normalizes_redirect_comparison() {
        let mut input = input("https://tokens.example.com/token".into());
        input.provider_config["request_headers"] = json!({"x-one": "a".repeat(6000), "x-two": "b".repeat(6000), "x-three": "c".repeat(6000)});
        let state = refresh_state(&input).unwrap();
        assert!(serde_json::to_vec(&state).unwrap().len() < 1024);
        assert!(state["authorization_code"].get("provider_config").is_none());
        input.client.client_secret = Some("x".repeat(17000));
        assert!(refresh_state(&input).is_err());
        assert_eq!(
            callback_code(
                "https://client.example/?code=fixture&state=fixture",
                "https://client.example:443",
                "fixture"
            )
            .unwrap(),
            "fixture"
        );
        assert_eq!(
            callback_code(
                "https://client.example/?code=fixture&state=fixture",
                "https://client.example",
                "fixture"
            )
            .unwrap(),
            "fixture"
        );
    }

    #[tokio::test]
    async fn rejected_proxy_or_native_config_precedes_login_and_transport_preflight_does_not_arm() {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("preflight.db").display()
        ))
        .await
        .unwrap();
        db.migrate().await.unwrap();
        let key = b"fixture encryption key at least 32 bytes long";
        let mut invalid = input("https://tokens.example.com/token".into());
        invalid.proxy_url = Some("socks5://192.168.1.10:1080".into());
        invalid.proxy_network_scope = Some(OutboundScope::Public);
        assert!(matches!(
            start(&db, invalid, key, 1000).await,
            Err(AppError::BadRequest(_))
        ));
        for configuration in [
            json!({"base_url": "https://api.example.com", "control_url": "ftp://api.example.com"}),
            json!({"base_url": "https://api.example.com", "request_headers": {"bad header": "fixture"}}),
        ] {
            let mut invalid = input("https://tokens.example.com/token".into());
            invalid.provider_driver = crate::provider::antigravity::DRIVER.into();
            invalid.provider_config = configuration;
            assert!(matches!(
                start(&db, invalid, key, 1000).await,
                Err(AppError::BadRequest(_))
            ));
        }
        let mut input = input("https://tokens.example.com/token".into());
        input.client.redirect_uri = "https://client.example:443".into();
        input.provider_config["request_headers"] = json!({"x-one": "a".repeat(6000), "x-two": "b".repeat(6000), "x-three": "c".repeat(6000)});
        let started = start(&db, input, key, 1000).await.unwrap();
        let login_url = Url::parse(&started.login_url).unwrap();
        assert!(
            login_url
                .query_pairs()
                .any(|(name, value)| name == "redirect_uri" && value == "https://client.example/")
        );
        let session: Session = open_private_json(&started.session_token, key, TOKEN_AAD).unwrap();
        let reference = OAuthLoginSessionReference {
            session_id: session.session_id,
            flow_kind: FLOW.into(),
            tenant_external_id: session.tenant_external_id,
            operator_service_id: None,
            expires_at: session.expires_at,
        };
        let OAuthLoginClaim::Claimed {
            lease_owner,
            state_ciphertext,
        } = db
            .claim_oauth_login_poll(&reference, 1001, 1)
            .await
            .unwrap()
        else {
            panic!("initial lease")
        };
        let mut login: LoginState = open_private_json(&state_ciphertext, key, STATE_AAD).unwrap();
        let mock = MockServer::start().await;
        login.input.adapter.poll_url = format!("{}/token", mock.uri());
        let callback = format!(
            "{}?code=fixture&state={}",
            login.input.client.redirect_uri, login.state
        );
        db.replace_oauth_login_poll_state(
            session.session_id,
            lease_owner,
            seal_private_json(&login, key, STATE_AAD).unwrap(),
        )
        .await
        .unwrap();
        db.release_oauth_login_poll(session.session_id, lease_owner, 1001)
            .await
            .unwrap();
        // Production transport rejects the loopback destination before any dispatch.
        let result = complete(
            &db,
            &reqwest::Client::new(),
            &started.session_token,
            &callback,
            None,
            None,
            key,
            2002,
            false,
        )
        .await;
        assert!(matches!(result, Err(AppError::BadRequest(_))));
        assert!(mock.received_requests().await.unwrap().is_empty());
        let OAuthLoginClaim::Claimed {
            state_ciphertext, ..
        } = db
            .claim_oauth_login_poll(&reference, 3003, 1)
            .await
            .unwrap()
        else {
            panic!("preflight remains retryable")
        };
        let login: LoginState = open_private_json(&state_ciphertext, key, STATE_AAD).unwrap();
        assert!(!login.exchange_started);
        let issued = credential(
            TokenResponse {
                access_token: "fixture-access".into(),
                refresh_token: Some("fixture-refresh".into()),
                expires_in: 3600,
                token_type: None,
            },
            &login.input,
            1000,
            None,
        )
        .unwrap();
        crate::provider::seal_credential(&issued, key).unwrap();
    }
    #[test]
    fn token_form_escapes_reserved_characters_without_changing_credentials() {
        let fields = [
            ("code", "fixture+code&scope=other /?".to_owned()),
            ("client_secret", "fixture=secret+%".to_owned()),
        ];
        let encoded = encode_token_form(&fields);
        let decoded = url::form_urlencoded::parse(encoded.as_bytes())
            .into_owned()
            .collect::<Vec<_>>();
        assert_eq!(
            decoded,
            fields
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value))
                .collect::<Vec<_>>()
        );
    }
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_string_contains, method, path},
    };

    fn input(token_endpoint: String) -> StartInput {
        StartInput {
            reauthorize: None,
            application_plugin_revision: Some(7),
            tenant_external_id: "fixture-tenant".into(),
            account_name: "fixture-account".into(),
            provider_driver: "fixture-provider".into(),
            provider_config: json!({"base_url": "https://api.example.com", "network_scope": "public"}),
            operator_service_id: None,
            client: ClientConfig {
                client_id: "fixture-client".into(),
                client_secret: Some("fixture-client-parameter".into()),
                redirect_uri: "http://127.0.0.1:51121/oauth-callback".into(),
                scopes: vec!["profile".into()],
            },
            adapter: OAuthAdapterContribution {
                api_version: "oauth-adapter-v1".into(),
                flow_kind: OAuthFlowKind::AuthorizationCodePkce,
                login_url: "https://accounts.example.com/authorize".into(),
                poll_url: token_endpoint.clone(),
                refresh_url: token_endpoint,
            },
            proxy_url: None,
            proxy_network_scope: None,
        }
    }

    #[tokio::test]
    async fn session_binds_operator_revision_and_never_exposes_client_parameter() {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("oauth.db").display()
        ))
        .await
        .unwrap();
        db.migrate().await.unwrap();
        let key = b"fixture encryption key at least 32 bytes long";
        let started = start(
            &db,
            input("https://tokens.example.com/token".into()),
            key,
            1000,
        )
        .await
        .unwrap();
        assert!(started.login_url.contains("code_challenge_method=S256"));
        assert!(!started.login_url.contains("fixture-client-parameter"));
        assert!(!started.session_token.contains("fixture-client-parameter"));
        assert_eq!(
            session_application_revision(
                &started.session_token,
                key,
                Some("fixture-tenant"),
                None,
                1001
            )
            .unwrap(),
            Some(7)
        );
        assert!(
            session_application_revision(
                &started.session_token,
                key,
                Some("other-tenant"),
                None,
                1001
            )
            .is_err()
        );
        assert!(
            session_application_revision(
                &started.session_token,
                key,
                None,
                Some(Uuid::now_v7()),
                1001
            )
            .is_err()
        );
        assert!(
            session_application_revision(
                &started.session_token,
                key,
                None,
                None,
                started.recovery_expires_at
            )
            .is_err()
        );
        let session: Session = open_private_json(&started.session_token, key, TOKEN_AAD).unwrap();
        let ready = ReadyLogin {
            reauthorize: None,
            application_plugin_revision: Some(8),
            session_id: session.session_id,
            tenant_external_id: session.tenant_external_id.clone(),
            account_name: "fixture".into(),
            provider_driver: "fixture".into(),
            provider_config: json!({}),
            refresh_url: "https://tokens.example.com/token".into(),
            credential: UpstreamCredential::None,
        };
        assert!(
            open_ready(
                &seal_private_json(&ready, key, READY_AAD).unwrap(),
                key,
                &session
            )
            .is_err()
        );
        let reference = OAuthLoginSessionReference {
            session_id: session.session_id,
            flow_kind: FLOW.into(),
            tenant_external_id: session.tenant_external_id.clone(),
            operator_service_id: None,
            expires_at: session.expires_at,
        };
        let OAuthLoginClaim::Claimed {
            lease_owner,
            state_ciphertext,
        } = db
            .claim_oauth_login_poll(&reference, 1001, 1)
            .await
            .unwrap()
        else {
            panic!("expected initial lease")
        };
        let mut login: LoginState = open_private_json(&state_ciphertext, key, STATE_AAD).unwrap();
        login.exchange_started = true;
        db.replace_oauth_login_poll_state(
            session.session_id,
            lease_owner,
            seal_private_json(&login, key, STATE_AAD).unwrap(),
        )
        .await
        .unwrap();
        let check_only = complete(
            &db,
            &reqwest::Client::new(),
            &started.session_token,
            "",
            Some("fixture-tenant"),
            None,
            key,
            31_002,
            false,
        )
        .await;
        assert!(matches!(check_only, Err(AppError::Conflict(_))));
        let OAuthLoginClaim::Claimed {
            lease_owner: check_owner,
            state_ciphertext,
        } = db
            .claim_oauth_login_poll(&reference, 32_003, 1)
            .await
            .unwrap()
        else {
            panic!("empty callback must not fail the session")
        };
        let state_after_check: LoginState =
            open_private_json(&state_ciphertext, key, STATE_AAD).unwrap();
        assert!(state_after_check.exchange_started);
        db.release_oauth_login_poll(session.session_id, check_owner, 32_003)
            .await
            .unwrap();
        let reclaimed = complete(
            &db,
            &reqwest::Client::new(),
            &started.session_token,
            "http://127.0.0.1:51121/oauth-callback?code=fixture&state=unused",
            Some("fixture-tenant"),
            None,
            key,
            33_004,
            false,
        )
        .await;
        assert!(
            matches!(reclaimed, Err(AppError::Conflict(_))),
            "a dispatched code exchange must not be retried after lease takeover"
        );
    }

    #[tokio::test]
    async fn refresh_is_form_encoded_preserves_identity_and_retains_missing_refresh_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("client_secret=fixture-client-parameter"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "new-fixture-access", "expires_in": 3600, "token_type": "Bearer"}))).expect(1).mount(&server).await;
        let input = input(format!("{}/token", server.uri()));
        let current = credential(
            TokenResponse {
                access_token: "old-fixture-access".into(),
                refresh_token: Some("fixture-refresh".into()),
                expires_in: 3600,
                token_type: Some("Bearer".into()),
            },
            &input,
            1000,
            None,
        )
        .unwrap();
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let refreshed = refresh(
            &http,
            &current,
            &input.adapter,
            2000,
            true,
            &super::super::TEST_OAUTH_REFRESH_REQUEST_GUARD,
        )
        .await
        .unwrap();
        match refreshed {
            UpstreamCredential::OAuth {
                access_token,
                refresh_token,
                expires_at,
                ..
            } => {
                assert_eq!(access_token, "new-fixture-access");
                assert_eq!(refresh_token.as_deref(), Some("fixture-refresh"));
                assert_eq!(expires_at, Some(3_602_000));
            }
            _ => panic!("expected OAuth credential"),
        }
    }
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
