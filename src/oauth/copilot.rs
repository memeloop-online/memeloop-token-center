use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_TYPE, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;
use uuid::Uuid;

use crate::{
    db::{BeginOAuthLoginSession, Database, OAuthLoginClaim, OAuthLoginSessionReference},
    error::AppError,
    network::{self, OutboundScope},
    provider::{UpstreamCredential, open_private_json, seal_private_json},
};

use super::OAuthReauthorizationTarget;

pub const PROVIDER_DRIVER: &str = "github-copilot";
pub const OAUTH_DRIVER: &str = "github_copilot_device";
pub const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
pub const DEFAULT_COPILOT_API_ENDPOINT: &str = "https://api.githubcopilot.com";
pub const BASE_URL: &str = DEFAULT_COPILOT_API_ENDPOINT;
pub const TOKEN_ENDPOINT: &str = "https://api.github.com/copilot_internal/v2/token";

const DEVICE_CODE_ENDPOINT: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_ENDPOINT: &str = "https://github.com/login/oauth/access_token";
const GITHUB_USER_ENDPOINT: &str = "https://api.github.com/user";
const COPILOT_TOKEN_ENDPOINT: &str = TOKEN_ENDPOINT;
const GITHUB_HOST: &str = "github.com";
const GITHUB_API_VERSION: &str = "2024-12-15";
const REQUESTED_SCOPE: &str = "repo workflow";
const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const SESSION_AAD: &[u8] = b"memeloop-token-center/github-copilot-device-login/v1";
const STATE_AAD: &[u8] = b"memeloop-token-center/github-copilot-device-state/v1";
const READY_AAD: &[u8] = b"memeloop-token-center/github-copilot-device-ready/v1";
const RESPONSE_LIMIT: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_POLL_SECONDS: u64 = 5;
const SLOW_DOWN_SECONDS: u64 = 5;
const MAX_DEVICE_LIFETIME_SECONDS: i64 = 60 * 60;
const DEFAULT_DEVICE_LIFETIME_SECONDS: i64 = 15 * 60;
const DEFAULT_REFRESH_SECONDS: i64 = 30 * 60;
const MAX_TOKEN_LIFETIME_SECONDS: i64 = 24 * 60 * 60;
const USER_AGENT_VALUE: &str = "memeloop-token-center";

#[derive(Clone, Debug)]
pub struct StartCopilotDeviceLogin {
    pub tenant_external_id: String,
    pub account_name: String,
    pub operator_service_id: Option<Uuid>,
    pub provider_config: Value,
    pub reauthorize: Option<OAuthReauthorizationTarget>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CopilotDeviceLoginStart {
    pub driver: &'static str,
    pub verification_url: String,
    pub user_code: String,
    pub session_token: String,
    pub expires_at: i64,
    pub poll_after_seconds: u64,
    pub security_notice: &'static str,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadyCopilotDeviceLogin {
    pub session_id: Uuid,
    pub tenant_external_id: String,
    pub account_name: String,
    pub provider_config: Value,
    pub credential: UpstreamCredential,
    pub stable_account_id: String,
    pub login: String,
    pub scope: String,
    pub api_endpoint: String,
    pub reauthorize: Option<OAuthReauthorizationTarget>,
}

#[derive(Clone, Debug)]
pub enum CopilotDevicePollResult {
    Pending {
        retry_after_seconds: u64,
    },
    Ready {
        lease_owner: Uuid,
        login: Box<ReadyCopilotDeviceLogin>,
    },
    Consumed {
        account_id: Uuid,
        tenant_external_id: String,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct CopilotDevicePollScope<'a> {
    pub required_tenant: Option<&'a str>,
    pub operator_service_id: Option<Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct LoginState {
    session_id: Uuid,
    tenant_external_id: String,
    account_name: String,
    provider_config: Value,
    operator_service_id: Option<Uuid>,
    device_code: String,
    poll_interval_seconds: u64,
    expires_at: i64,
    reauthorize: Option<OAuthReauthorizationTarget>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SessionToken {
    session_id: Uuid,
    tenant_external_id: String,
    operator_service_id: Option<Uuid>,
    poll_interval_seconds: u64,
    expires_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct AdapterState {
    schema: String,
    github_token: String,
    github_host: String,
    github_user_id: String,
    stable_account_id: String,
    login: String,
    scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    github_token_expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    github_refresh_token_expires_at: Option<i64>,
    refresh_in: i64,
    copilot_api_endpoint: String,
}

#[derive(Clone, Debug)]
struct Endpoints {
    device_code: String,
    access_token: String,
    github_user: String,
    copilot_token: String,
}

impl Endpoints {
    fn production() -> Self {
        Self {
            device_code: DEVICE_CODE_ENDPOINT.into(),
            access_token: ACCESS_TOKEN_ENDPOINT.into(),
            github_user: GITHUB_USER_ENDPOINT.into(),
            copilot_token: COPILOT_TOKEN_ENDPOINT.into(),
        }
    }

    #[cfg(test)]
    fn test(origin: &str) -> Self {
        Self {
            device_code: format!("{origin}/login/device/code"),
            access_token: format!("{origin}/login/oauth/access_token"),
            github_user: format!("{origin}/user"),
            copilot_token: format!("{origin}/copilot_internal/v2/token"),
        }
    }
}

struct PollRuntime<'a> {
    key_material: &'a [u8],
    now: i64,
    scope: CopilotDevicePollScope<'a>,
    allow_test_loopback: bool,
    endpoints: &'a Endpoints,
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct DeviceTokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    refresh_token_expires_in: Option<i64>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct GitHubUser {
    id: u64,
    login: String,
}

#[derive(Deserialize)]
struct CopilotTokenResponse {
    token: String,
    expires_at: i64,
    #[serde(default)]
    refresh_in: Option<i64>,
    #[serde(default)]
    endpoints: Option<CopilotTokenEndpoints>,
}

#[derive(Deserialize)]
struct CopilotTokenEndpoints {
    #[serde(default)]
    api: Option<String>,
}

#[derive(Deserialize)]
struct GitHubRefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    refresh_token_expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug)]
struct ExchangedToken {
    access_token: String,
    expires_at: i64,
    refresh_in: i64,
    api_endpoint: String,
}

enum ExchangeError {
    Unauthorized,
    Failed,
}

pub async fn start_copilot_device_login(
    db: &Database,
    http: &reqwest::Client,
    input: StartCopilotDeviceLogin,
    key_material: &[u8],
    now: i64,
    allow_test_loopback: bool,
) -> Result<CopilotDeviceLoginStart, AppError> {
    start_at(
        db,
        http,
        input,
        key_material,
        now,
        allow_test_loopback,
        &Endpoints::production(),
    )
    .await
}

async fn start_at(
    db: &Database,
    http: &reqwest::Client,
    input: StartCopilotDeviceLogin,
    key_material: &[u8],
    now: i64,
    allow_test_loopback: bool,
    endpoints: &Endpoints,
) -> Result<CopilotDeviceLoginStart, AppError> {
    validate_account_text(&input.tenant_external_id, "tenant")?;
    validate_account_text(input.account_name.trim(), "account name")?;
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", CLIENT_ID)
        .append_pair("scope", REQUESTED_SCOPE)
        .finish();
    let response = oauth_client(http, &endpoints.device_code, allow_test_loopback)
        .await?
        .post(&endpoints.device_code)
        .header(ACCEPT, "application/json")
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(USER_AGENT, USER_AGENT_VALUE)
        .body(form)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|_| upstream_error())?;
    if !response.status().is_success() {
        return Err(upstream_error());
    }
    let response: DeviceCodeResponse =
        serde_json::from_slice(&bounded_body(response).await?).map_err(|_| upstream_error())?;
    validate_secret(&response.device_code)?;
    validate_user_code(&response.user_code)?;
    validate_verification_uri(&response.verification_uri, allow_test_loopback)?;
    let poll_interval_seconds = response.interval.unwrap_or(DEFAULT_POLL_SECONDS);
    if poll_interval_seconds == 0 {
        return Err(upstream_error());
    }
    let lifetime = response
        .expires_in
        .unwrap_or(DEFAULT_DEVICE_LIFETIME_SECONDS);
    if !(1..=MAX_DEVICE_LIFETIME_SECONDS).contains(&lifetime) {
        return Err(upstream_error());
    }
    let expires_at = checked_expiry(now, lifetime)?;
    let next_poll_at = now.saturating_add(seconds_millis(poll_interval_seconds));
    if next_poll_at >= expires_at {
        return Err(upstream_error());
    }
    let session_id = Uuid::now_v7();
    let state = LoginState {
        session_id,
        tenant_external_id: input.tenant_external_id,
        account_name: input.account_name.trim().to_owned(),
        provider_config: input.provider_config,
        operator_service_id: input.operator_service_id,
        device_code: response.device_code,
        poll_interval_seconds,
        expires_at,
        reauthorize: input.reauthorize,
    };
    let token = SessionToken {
        session_id,
        tenant_external_id: state.tenant_external_id.clone(),
        operator_service_id: state.operator_service_id,
        poll_interval_seconds,
        expires_at,
    };
    db.begin_oauth_login_session(BeginOAuthLoginSession {
        session_id,
        flow_kind: OAUTH_DRIVER.to_owned(),
        tenant_external_id: state.tenant_external_id.clone(),
        operator_service_id: state.operator_service_id,
        state_ciphertext: seal_private_json(&state, key_material, STATE_AAD)?,
        next_poll_at,
        expires_at,
    })
    .await?;
    Ok(CopilotDeviceLoginStart {
        driver: PROVIDER_DRIVER,
        verification_url: response.verification_uri,
        user_code: response.user_code,
        session_token: seal_private_json(&token, key_material, SESSION_AAD)?,
        expires_at,
        poll_after_seconds: poll_interval_seconds,
        security_notice: "only_continue_if_you_started_this_login",
    })
}

pub async fn poll_copilot_device_login(
    db: &Database,
    http: &reqwest::Client,
    session_token: &str,
    key_material: &[u8],
    now: i64,
    scope: CopilotDevicePollScope<'_>,
    allow_test_loopback: bool,
) -> Result<CopilotDevicePollResult, AppError> {
    poll_at(
        db,
        http,
        session_token,
        PollRuntime {
            key_material,
            now,
            scope,
            allow_test_loopback,
            endpoints: &Endpoints::production(),
        },
    )
    .await
}

async fn poll_at(
    db: &Database,
    http: &reqwest::Client,
    session_token: &str,
    runtime: PollRuntime<'_>,
) -> Result<CopilotDevicePollResult, AppError> {
    let PollRuntime {
        key_material,
        now,
        scope,
        allow_test_loopback,
        endpoints,
    } = runtime;
    let session: SessionToken = open_private_json(session_token, key_material, SESSION_AAD)
        .map_err(|_| AppError::BadRequest("invalid OAuth session token".into()))?;
    if scope
        .required_tenant
        .is_some_and(|tenant| tenant != session.tenant_external_id)
        || scope.operator_service_id != session.operator_service_id
    {
        return Err(AppError::Forbidden);
    }
    if session.expires_at <= now {
        return Err(AppError::BadRequest("OAuth login session expired".into()));
    }
    let reference = OAuthLoginSessionReference {
        session_id: session.session_id,
        flow_kind: OAUTH_DRIVER.to_owned(),
        tenant_external_id: session.tenant_external_id.clone(),
        operator_service_id: session.operator_service_id,
        expires_at: session.expires_at,
    };
    let (lease_owner, mut state) = match db
        .claim_oauth_login_poll(&reference, now, session.poll_interval_seconds)
        .await?
    {
        OAuthLoginClaim::Pending {
            retry_after_seconds,
        } => {
            return Ok(CopilotDevicePollResult::Pending {
                retry_after_seconds,
            });
        }
        OAuthLoginClaim::Consumed { account_id } => {
            return Ok(CopilotDevicePollResult::Consumed {
                account_id,
                tenant_external_id: session.tenant_external_id,
            });
        }
        OAuthLoginClaim::Ready {
            lease_owner,
            ready_ciphertext,
        } => {
            return Ok(CopilotDevicePollResult::Ready {
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
        } => (
            lease_owner,
            open_private_json::<LoginState>(&state_ciphertext, key_material, STATE_AAD)?,
        ),
    };
    if state.session_id != session.session_id || state.expires_at != session.expires_at {
        return Err(AppError::Forbidden);
    }
    let device_result =
        poll_device_token(http, &state.device_code, allow_test_loopback, endpoints).await;
    let device = match device_result {
        Ok(DeviceOutcome::Pending { interval }) => {
            let next_interval = interval
                .filter(|interval| *interval > 0)
                .unwrap_or(state.poll_interval_seconds)
                .max(state.poll_interval_seconds);
            reschedule_poll(
                db,
                &mut state,
                lease_owner,
                next_interval,
                key_material,
                now,
            )
            .await?;
            return Ok(CopilotDevicePollResult::Pending {
                retry_after_seconds: next_interval,
            });
        }
        Ok(DeviceOutcome::SlowDown { interval }) => {
            let increased = state
                .poll_interval_seconds
                .checked_add(SLOW_DOWN_SECONDS)
                .ok_or_else(upstream_error)?;
            let next_interval = interval
                .filter(|interval| *interval > 0)
                .unwrap_or(0)
                .max(increased);
            reschedule_poll(
                db,
                &mut state,
                lease_owner,
                next_interval,
                key_material,
                now,
            )
            .await?;
            return Ok(CopilotDevicePollResult::Pending {
                retry_after_seconds: next_interval,
            });
        }
        Ok(DeviceOutcome::Denied) => {
            db.fail_oauth_login_poll(state.session_id, lease_owner, now)
                .await?;
            return Err(AppError::BadRequest(
                "GitHub Copilot authorization was denied".into(),
            ));
        }
        Ok(DeviceOutcome::Expired) => {
            db.fail_oauth_login_poll(state.session_id, lease_owner, now)
                .await?;
            return Err(AppError::BadRequest(
                "GitHub Copilot authorization expired".into(),
            ));
        }
        Ok(DeviceOutcome::Terminal) => {
            db.fail_oauth_login_poll(state.session_id, lease_owner, now)
                .await?;
            return Err(AppError::BadRequest(
                "GitHub Copilot authorization cannot continue".into(),
            ));
        }
        Ok(DeviceOutcome::Ready(tokens)) => tokens,
        Err(error) => {
            let interval = state.poll_interval_seconds;
            reschedule_poll(db, &mut state, lease_owner, interval, key_material, now).await?;
            return Err(error);
        }
    };
    let github_token = device.access_token.as_deref().ok_or_else(upstream_error)?;
    validate_secret(github_token)?;
    if device
        .token_type
        .as_deref()
        .is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer"))
    {
        return Err(upstream_error());
    }
    let user = fetch_user(http, github_token, allow_test_loopback, endpoints).await?;
    let stable_account_id = stable_identity(user.id)?;
    let copilot = exchange_copilot(http, github_token, now, allow_test_loopback, endpoints).await?;
    let scope = normalized_scope(device.scope.as_deref().unwrap_or(REQUESTED_SCOPE))?;
    let refresh_token = validated_optional_secret(device.refresh_token)?;
    let adapter_state = AdapterState {
        schema: "github-copilot-oauth-v1".into(),
        github_token: github_token.to_owned(),
        github_host: GITHUB_HOST.into(),
        github_user_id: user.id.to_string(),
        stable_account_id: stable_account_id.clone(),
        login: user.login.clone(),
        scope: scope.clone(),
        github_token_expires_at: optional_expiry(now, device.expires_in)?,
        github_refresh_token_expires_at: optional_expiry(now, device.refresh_token_expires_in)?,
        refresh_in: copilot.refresh_in,
        copilot_api_endpoint: copilot.api_endpoint.clone(),
    };
    let provider_config =
        authoritative_provider_config(state.provider_config, &copilot.api_endpoint)?;
    let ready = ReadyCopilotDeviceLogin {
        session_id: state.session_id,
        tenant_external_id: state.tenant_external_id,
        account_name: state.account_name,
        provider_config,
        credential: UpstreamCredential::OAuth {
            access_token: copilot.access_token,
            refresh_token,
            expires_at: Some(copilot.expires_at),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: Some(
                serde_json::to_value(adapter_state).map_err(|_| AppError::Internal)?,
            ),
        },
        stable_account_id,
        login: user.login,
        scope,
        api_endpoint: copilot.api_endpoint,
        reauthorize: state.reauthorize,
    };
    db.stage_oauth_login_ready(
        state.session_id,
        lease_owner,
        seal_private_json(&ready, key_material, READY_AAD)?,
        now,
    )
    .await?;
    match db
        .claim_oauth_login_poll(&reference, now, session.poll_interval_seconds)
        .await?
    {
        OAuthLoginClaim::Ready {
            lease_owner,
            ready_ciphertext,
        } => Ok(CopilotDevicePollResult::Ready {
            lease_owner,
            login: Box::new(open_private_json(
                &ready_ciphertext,
                key_material,
                READY_AAD,
            )?),
        }),
        OAuthLoginClaim::Consumed { account_id } => Ok(CopilotDevicePollResult::Consumed {
            account_id,
            tenant_external_id: session.tenant_external_id,
        }),
        OAuthLoginClaim::Pending {
            retry_after_seconds,
        } => Ok(CopilotDevicePollResult::Pending {
            retry_after_seconds,
        }),
        OAuthLoginClaim::Claimed { .. } => Err(AppError::Internal),
    }
}

async fn reschedule_poll(
    db: &Database,
    state: &mut LoginState,
    lease_owner: Uuid,
    next_interval_seconds: u64,
    key_material: &[u8],
    now: i64,
) -> Result<(), AppError> {
    if next_interval_seconds == 0 {
        return Err(upstream_error());
    }
    let next_poll_at = now
        .checked_add(
            i64::try_from(next_interval_seconds)
                .ok()
                .and_then(|seconds| seconds.checked_mul(1_000))
                .ok_or_else(upstream_error)?,
        )
        .ok_or_else(upstream_error)?;
    if next_poll_at >= state.expires_at {
        db.fail_oauth_login_poll(state.session_id, lease_owner, now)
            .await?;
        return Err(AppError::BadRequest(
            "GitHub Copilot authorization expired".into(),
        ));
    }
    state.poll_interval_seconds = next_interval_seconds;
    db.reschedule_oauth_login_poll(
        state.session_id,
        lease_owner,
        seal_private_json(state, key_material, STATE_AAD)?,
        next_poll_at,
        now,
    )
    .await
}

enum DeviceOutcome {
    Pending { interval: Option<u64> },
    SlowDown { interval: Option<u64> },
    Denied,
    Expired,
    Terminal,
    Ready(DeviceTokenResponse),
}

async fn poll_device_token(
    http: &reqwest::Client,
    device_code: &str,
    allow_test_loopback: bool,
    endpoints: &Endpoints,
) -> Result<DeviceOutcome, AppError> {
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", CLIENT_ID)
        .append_pair("device_code", device_code)
        .append_pair("grant_type", DEVICE_GRANT_TYPE)
        .finish();
    let response = oauth_client(http, &endpoints.access_token, allow_test_loopback)
        .await?
        .post(&endpoints.access_token)
        .header(ACCEPT, "application/json")
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(USER_AGENT, USER_AGENT_VALUE)
        .body(form)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|_| upstream_error())?;
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(AppError::RateLimited);
    }
    if response.status().is_client_error() {
        return Ok(DeviceOutcome::Terminal);
    }
    if !response.status().is_success() {
        return Err(upstream_error());
    }
    let response: DeviceTokenResponse =
        serde_json::from_slice(&bounded_body(response).await?).map_err(|_| upstream_error())?;
    match response.error.as_deref() {
        Some("authorization_pending") => Ok(DeviceOutcome::Pending {
            interval: response.interval,
        }),
        Some("slow_down") => Ok(DeviceOutcome::SlowDown {
            interval: response.interval,
        }),
        Some("access_denied") => Ok(DeviceOutcome::Denied),
        Some("expired_token") => Ok(DeviceOutcome::Expired),
        Some(
            "incorrect_client_credentials"
            | "incorrect_device_code"
            | "device_flow_disabled"
            | "unsupported_grant_type",
        ) => Ok(DeviceOutcome::Terminal),
        Some(_) => Err(upstream_error()),
        None if response.access_token.is_some() => Ok(DeviceOutcome::Ready(response)),
        None => Err(upstream_error()),
    }
}

async fn fetch_user(
    http: &reqwest::Client,
    github_token: &str,
    allow_test_loopback: bool,
    endpoints: &Endpoints,
) -> Result<GitHubUser, AppError> {
    let response = oauth_client(http, &endpoints.github_user, allow_test_loopback)
        .await?
        .get(&endpoints.github_user)
        .header(ACCEPT, "application/vnd.github+json")
        .header(USER_AGENT, USER_AGENT_VALUE)
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .bearer_auth(github_token)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|_| upstream_error())?;
    if !response.status().is_success() {
        return Err(upstream_error());
    }
    let user: GitHubUser =
        serde_json::from_slice(&bounded_body(response).await?).map_err(|_| upstream_error())?;
    if user.id == 0 {
        return Err(upstream_error());
    }
    validate_login(&user.login)?;
    Ok(user)
}

async fn exchange_copilot(
    http: &reqwest::Client,
    github_token: &str,
    now: i64,
    allow_test_loopback: bool,
    endpoints: &Endpoints,
) -> Result<ExchangedToken, AppError> {
    exchange_copilot_inner(http, github_token, now, allow_test_loopback, endpoints)
        .await
        .map_err(|_| upstream_error())
}

async fn exchange_copilot_inner(
    http: &reqwest::Client,
    github_token: &str,
    now: i64,
    allow_test_loopback: bool,
    endpoints: &Endpoints,
) -> Result<ExchangedToken, ExchangeError> {
    let response = oauth_client(http, &endpoints.copilot_token, allow_test_loopback)
        .await
        .map_err(|_| ExchangeError::Failed)?
        .get(&endpoints.copilot_token)
        .header(ACCEPT, "application/json")
        .header(USER_AGENT, USER_AGENT_VALUE)
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .bearer_auth(github_token)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|_| ExchangeError::Failed)?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(ExchangeError::Unauthorized);
    }
    if !response.status().is_success() {
        return Err(ExchangeError::Failed);
    }
    let token: CopilotTokenResponse = serde_json::from_slice(
        &bounded_body(response)
            .await
            .map_err(|_| ExchangeError::Failed)?,
    )
    .map_err(|_| ExchangeError::Failed)?;
    validate_secret(&token.token).map_err(|_| ExchangeError::Failed)?;
    let expires_at = absolute_expiry(token.expires_at, now).map_err(|_| ExchangeError::Failed)?;
    let refresh_in = token
        .refresh_in
        .unwrap_or(DEFAULT_REFRESH_SECONDS)
        .clamp(1, MAX_TOKEN_LIFETIME_SECONDS);
    let api_endpoint = token
        .endpoints
        .and_then(|endpoints| endpoints.api)
        .unwrap_or_else(|| DEFAULT_COPILOT_API_ENDPOINT.to_owned());
    validate_https_endpoint(&api_endpoint).map_err(|_| ExchangeError::Failed)?;
    Ok(ExchangedToken {
        access_token: token.token,
        expires_at,
        refresh_in,
        api_endpoint,
    })
}

pub async fn refresh_copilot_credential(
    http: &reqwest::Client,
    credential: &UpstreamCredential,
    now: i64,
    allow_test_loopback: bool,
) -> Result<UpstreamCredential, AppError> {
    refresh_at(
        http,
        credential,
        now,
        allow_test_loopback,
        &Endpoints::production(),
    )
    .await
}

pub fn copilot_account_id(credential: &UpstreamCredential) -> Result<String, AppError> {
    let UpstreamCredential::OAuth {
        adapter_state: Some(value),
        ..
    } = credential
    else {
        return Err(invalid_state());
    };
    let state: AdapterState = serde_json::from_value(value.clone()).map_err(|_| invalid_state())?;
    validate_state(&state)?;
    Ok(state.stable_account_id)
}

async fn refresh_at(
    http: &reqwest::Client,
    credential: &UpstreamCredential,
    now: i64,
    allow_test_loopback: bool,
    endpoints: &Endpoints,
) -> Result<UpstreamCredential, AppError> {
    let UpstreamCredential::OAuth {
        refresh_token,
        adapter_state: Some(value),
        ..
    } = credential
    else {
        return Err(AppError::BadRequest(
            "upstream account has no GitHub Copilot OAuth state".into(),
        ));
    };
    let mut state: AdapterState =
        serde_json::from_value(value.clone()).map_err(|_| invalid_state())?;
    validate_state(&state)?;
    let copilot = exchange_copilot_inner(
        http,
        &state.github_token,
        now,
        allow_test_loopback,
        endpoints,
    )
    .await;
    let (copilot, next_refresh_token) = match copilot {
        Ok(token) => (token, refresh_token.clone()),
        Err(ExchangeError::Unauthorized)
            if refresh_token
                .as_deref()
                .is_some_and(|token| !token.is_empty()) =>
        {
            let refreshed = refresh_github(
                http,
                refresh_token.as_deref().unwrap_or_default(),
                allow_test_loopback,
                endpoints,
            )
            .await?;
            validate_secret(&refreshed.access_token)?;
            state.github_token = refreshed.access_token;
            state.scope = normalized_scope(refreshed.scope.as_deref().unwrap_or(&state.scope))?;
            state.github_token_expires_at = optional_expiry(now, refreshed.expires_in)?;
            state.github_refresh_token_expires_at =
                optional_expiry(now, refreshed.refresh_token_expires_in)?;
            let next_refresh = validated_optional_secret(refreshed.refresh_token)?
                .or_else(|| refresh_token.clone());
            let token = exchange_copilot(
                http,
                &state.github_token,
                now,
                allow_test_loopback,
                endpoints,
            )
            .await?;
            (token, next_refresh)
        }
        Err(_) => return Err(upstream_error()),
    };
    state.refresh_in = copilot.refresh_in;
    state.copilot_api_endpoint = copilot.api_endpoint;
    Ok(UpstreamCredential::OAuth {
        access_token: copilot.access_token,
        refresh_token: next_refresh_token,
        expires_at: Some(copilot.expires_at),
        header: "authorization".into(),
        prefix: "Bearer ".into(),
        adapter_state: Some(serde_json::to_value(state).map_err(|_| AppError::Internal)?),
    })
}

async fn refresh_github(
    http: &reqwest::Client,
    refresh_token: &str,
    allow_test_loopback: bool,
    endpoints: &Endpoints,
) -> Result<GitHubRefreshResponse, AppError> {
    validate_secret(refresh_token)?;
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", CLIENT_ID)
        .append_pair("grant_type", "refresh_token")
        .append_pair("refresh_token", refresh_token)
        .finish();
    let response = oauth_client(http, &endpoints.access_token, allow_test_loopback)
        .await?
        .post(&endpoints.access_token)
        .header(ACCEPT, "application/json")
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(USER_AGENT, USER_AGENT_VALUE)
        .body(form)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|_| upstream_error())?;
    if !response.status().is_success() {
        return Err(upstream_error());
    }
    serde_json::from_slice(&bounded_body(response).await?).map_err(|_| upstream_error())
}

async fn oauth_client(
    http: &reqwest::Client,
    endpoint: &str,
    allow_test_loopback: bool,
) -> Result<reqwest::Client, AppError> {
    network::client_for_url(http, endpoint, OutboundScope::Public, allow_test_loopback)
        .await
        .map_err(|_| upstream_error())
}

async fn bounded_body(response: reqwest::Response) -> Result<Vec<u8>, AppError> {
    if response
        .content_length()
        .is_some_and(|length| length > RESPONSE_LIMIT as u64)
    {
        return Err(upstream_error());
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| upstream_error())?;
        if body.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
            return Err(upstream_error());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn validate_state(state: &AdapterState) -> Result<(), AppError> {
    let id = state
        .github_user_id
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(invalid_state)?;
    if state.schema != "github-copilot-oauth-v1"
        || state.github_host != GITHUB_HOST
        || stable_identity(id).map_err(|_| invalid_state())? != state.stable_account_id
    {
        return Err(invalid_state());
    }
    validate_secret(&state.github_token).map_err(|_| invalid_state())?;
    validate_login(&state.login).map_err(|_| invalid_state())?;
    normalized_scope(&state.scope).map_err(|_| invalid_state())?;
    validate_https_endpoint(&state.copilot_api_endpoint).map_err(|_| invalid_state())?;
    Ok(())
}

fn authoritative_provider_config(mut config: Value, api_endpoint: &str) -> Result<Value, AppError> {
    validate_https_endpoint(api_endpoint)?;
    let object = config.as_object_mut().ok_or_else(|| {
        AppError::BadRequest("GitHub Copilot provider configuration is invalid".into())
    })?;
    object.insert("base_url".into(), Value::String(api_endpoint.to_owned()));
    Ok(config)
}

fn stable_identity(id: u64) -> Result<String, AppError> {
    if id == 0 {
        return Err(upstream_error());
    }
    Ok(format!("{GITHUB_HOST}:{id}"))
}

fn normalized_scope(scope: &str) -> Result<String, AppError> {
    if scope.len() > 2_048 || scope.chars().any(char::is_control) {
        return Err(upstream_error());
    }
    let mut scopes = scope
        .split(|character: char| character == ',' || character.is_ascii_whitespace())
        .filter(|scope| !scope.is_empty())
        .collect::<Vec<_>>();
    scopes.sort_unstable();
    scopes.dedup();
    if scopes.is_empty() {
        return Err(upstream_error());
    }
    Ok(scopes.join(" "))
}

fn validate_verification_uri(value: &str, allow_loopback: bool) -> Result<(), AppError> {
    let parsed = Url::parse(value).map_err(|_| upstream_error())?;
    let allowed_loopback = allow_loopback
        && parsed.scheme() == "http"
        && parsed
            .host_str()
            .is_some_and(|host| matches!(host, "127.0.0.1" | "::1" | "localhost"));
    if (parsed.scheme() != "https" && !allowed_loopback)
        || !parsed.has_host()
        || parsed.username() != ""
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(upstream_error());
    }
    Ok(())
}

fn validate_https_endpoint(value: &str) -> Result<(), AppError> {
    let parsed = Url::parse(value).map_err(|_| upstream_error())?;
    if parsed.scheme() != "https"
        || !parsed.has_host()
        || parsed.username() != ""
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(upstream_error());
    }
    Ok(())
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

fn validate_login(value: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(upstream_error());
    }
    Ok(())
}

fn validate_secret(value: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > 128 * 1024
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(upstream_error());
    }
    Ok(())
}

fn validated_optional_secret(value: Option<String>) -> Result<Option<String>, AppError> {
    match value {
        Some(value) if !value.is_empty() => {
            validate_secret(&value)?;
            Ok(Some(value))
        }
        _ => Ok(None),
    }
}

fn validate_user_code(value: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(upstream_error());
    }
    Ok(())
}

fn optional_expiry(now: i64, seconds: Option<i64>) -> Result<Option<i64>, AppError> {
    seconds
        .map(|seconds| checked_expiry(now, seconds))
        .transpose()
}

fn checked_expiry(now: i64, seconds: i64) -> Result<i64, AppError> {
    if !(1..=365 * 24 * 60 * 60).contains(&seconds) {
        return Err(upstream_error());
    }
    now.checked_add(seconds.checked_mul(1_000).ok_or_else(upstream_error)?)
        .ok_or_else(upstream_error)
}

fn absolute_expiry(value: i64, now: i64) -> Result<i64, AppError> {
    let millis = if value < 10_000_000_000 {
        value.checked_mul(1_000).ok_or_else(upstream_error)?
    } else {
        value
    };
    if millis <= now
        || millis > now.saturating_add(MAX_TOKEN_LIFETIME_SECONDS.saturating_mul(1_000))
    {
        return Err(upstream_error());
    }
    Ok(millis)
}

fn seconds_millis(seconds: u64) -> i64 {
    i64::try_from(seconds)
        .unwrap_or(i64::MAX)
        .saturating_mul(1_000)
}

fn invalid_state() -> AppError {
    AppError::BadRequest("invalid GitHub Copilot OAuth state".into())
}

fn upstream_error() -> AppError {
    AppError::Upstream("GitHub Copilot authorization failed".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::CreateUpstreamAccountInput;
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_string_contains, header, method, path},
    };

    const NOW: i64 = 1_700_000_000_000;
    const KEY: &[u8] = b"test material with at least 32 bytes";

    async fn sqlite_database() -> (tempfile::TempDir, String, Database) {
        let directory = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("copilot.db").display()
        );
        let database = Database::connect(&url).await.unwrap();
        database.migrate().await.unwrap();
        (directory, url, database)
    }

    fn input() -> StartCopilotDeviceLogin {
        StartCopilotDeviceLogin {
            tenant_external_id: "tenant-a".into(),
            account_name: "octocat-copilot".into(),
            operator_service_id: None,
            provider_config: json!({
                "base_url": DEFAULT_COPILOT_API_ENDPOINT,
                "network_scope": "public",
                "reservation_token_bounds": {}
            }),
            reauthorize: Some(OAuthReauthorizationTarget {
                account_id: Uuid::nil(),
                expected_updated_at: 42,
            }),
        }
    }

    async fn mount_start(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/login/device/code"))
            .and(body_string_contains(format!("client_id={CLIENT_ID}")))
            .and(body_string_contains("scope=repo+workflow"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "device_code": "github-device-secret",
                "user_code": "ABCD-1234",
                "verification_uri": "https://github.com/login/device",
                "expires_in": 900,
                "interval": 1
            })))
            .expect(1)
            .mount(server)
            .await;
    }

    fn oauth_state(stable_account_id: &str) -> Value {
        json!({
            "schema": "github-copilot-oauth-v1",
            "github_token": "github-raw-secret",
            "github_host": "github.com",
            "github_user_id": "12345",
            "stable_account_id": stable_account_id,
            "login": "octocat",
            "scope": "repo workflow",
            "refresh_in": 900,
            "copilot_api_endpoint": DEFAULT_COPILOT_API_ENDPOINT
        })
    }

    fn credential(state: Value) -> UpstreamCredential {
        UpstreamCredential::OAuth {
            access_token: "old-copilot-token".into(),
            refresh_token: None,
            expires_at: Some(NOW + 1_000),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: Some(state),
        }
    }

    #[tokio::test]
    async fn start_uses_github_form_scope_and_persists_encrypted_state() {
        let server = MockServer::start().await;
        mount_start(&server).await;
        let (_directory, _url, database) = sqlite_database().await;
        let started = start_at(
            &database,
            &reqwest::Client::new(),
            input(),
            KEY,
            NOW,
            true,
            &Endpoints::test(&server.uri()),
        )
        .await
        .unwrap();
        assert_eq!(started.driver, PROVIDER_DRIVER);
        assert_eq!(started.user_code, "ABCD-1234");
        assert_eq!(started.poll_after_seconds, 1);
        assert_eq!(started.expires_at, NOW + 900_000);
        assert!(!started.session_token.contains("github-device-secret"));
        assert!(!started.session_token.contains("tenant-a"));
    }

    #[tokio::test]
    async fn device_poll_maps_pending_slow_down_denied_and_expired() {
        for (error, expected) in [
            ("authorization_pending", "pending"),
            ("slow_down", "slow"),
            ("access_denied", "denied"),
            ("expired_token", "expired"),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/login/oauth/access_token"))
                .and(body_string_contains(format!("client_id={CLIENT_ID}")))
                .and(body_string_contains("device_code=device-secret"))
                .and(body_string_contains(
                    "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code",
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"error": error})))
                .mount(&server)
                .await;
            let result = poll_device_token(
                &reqwest::Client::new(),
                "device-secret",
                true,
                &Endpoints::test(&server.uri()),
            )
            .await
            .unwrap();
            assert!(matches!(
                (expected, result),
                ("pending", DeviceOutcome::Pending { .. })
                    | ("slow", DeviceOutcome::SlowDown { .. })
                    | ("denied", DeviceOutcome::Denied)
                    | ("expired", DeviceOutcome::Expired)
            ));
        }
    }

    #[tokio::test]
    async fn slow_down_interval_is_cumulative_durable_and_never_clamped_down() {
        let server = MockServer::start().await;
        mount_start(&server).await;
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "error": "slow_down",
                "interval": 75
            })))
            .mount(&server)
            .await;
        let (_directory, url, database) = sqlite_database().await;
        let started = start_at(
            &database,
            &reqwest::Client::new(),
            input(),
            KEY,
            NOW,
            true,
            &Endpoints::test(&server.uri()),
        )
        .await
        .unwrap();
        let first_poll_at = NOW + 1_000;
        let first = poll_at(
            &database,
            &reqwest::Client::new(),
            &started.session_token,
            PollRuntime {
                key_material: KEY,
                now: first_poll_at,
                scope: CopilotDevicePollScope {
                    required_tenant: Some("tenant-a"),
                    operator_service_id: None,
                },
                allow_test_loopback: true,
                endpoints: &Endpoints::test(&server.uri()),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            first,
            CopilotDevicePollResult::Pending {
                retry_after_seconds: 75
            }
        ));
        drop(database);
        let restarted = Database::connect(&url).await.unwrap();
        let early = poll_at(
            &restarted,
            &reqwest::Client::new(),
            &started.session_token,
            PollRuntime {
                key_material: KEY,
                now: first_poll_at + 74_000,
                scope: CopilotDevicePollScope {
                    required_tenant: Some("tenant-a"),
                    operator_service_id: None,
                },
                allow_test_loopback: true,
                endpoints: &Endpoints::test(&server.uri()),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            early,
            CopilotDevicePollResult::Pending {
                retry_after_seconds: 1
            }
        ));
        let second = poll_at(
            &restarted,
            &reqwest::Client::new(),
            &started.session_token,
            PollRuntime {
                key_material: KEY,
                now: first_poll_at + 75_000,
                scope: CopilotDevicePollScope {
                    required_tenant: Some("tenant-a"),
                    operator_service_id: None,
                },
                allow_test_loopback: true,
                endpoints: &Endpoints::test(&server.uri()),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            second,
            CopilotDevicePollResult::Pending {
                retry_after_seconds: 80
            }
        ));
        let token_polls = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| request.url.path() == "/login/oauth/access_token")
            .count();
        assert_eq!(token_polls, 2);
    }

    #[tokio::test]
    async fn denied_and_expired_device_sessions_are_terminal() {
        for (oauth_error, expected_message) in
            [("access_denied", "denied"), ("expired_token", "expired")]
        {
            let server = MockServer::start().await;
            mount_start(&server).await;
            Mock::given(method("POST"))
                .and(path("/login/oauth/access_token"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"error": oauth_error})),
                )
                .mount(&server)
                .await;
            let (_directory, _url, database) = sqlite_database().await;
            let started = start_at(
                &database,
                &reqwest::Client::new(),
                input(),
                KEY,
                NOW,
                true,
                &Endpoints::test(&server.uri()),
            )
            .await
            .unwrap();
            let first_error = poll_at(
                &database,
                &reqwest::Client::new(),
                &started.session_token,
                PollRuntime {
                    key_material: KEY,
                    now: NOW + 1_000,
                    scope: CopilotDevicePollScope {
                        required_tenant: Some("tenant-a"),
                        operator_service_id: None,
                    },
                    allow_test_loopback: true,
                    endpoints: &Endpoints::test(&server.uri()),
                },
            )
            .await
            .unwrap_err();
            assert!(first_error.to_string().contains(expected_message));
            let second_error = poll_at(
                &database,
                &reqwest::Client::new(),
                &started.session_token,
                PollRuntime {
                    key_material: KEY,
                    now: NOW + 2_000,
                    scope: CopilotDevicePollScope {
                        required_tenant: Some("tenant-a"),
                        operator_service_id: None,
                    },
                    allow_test_loopback: true,
                    endpoints: &Endpoints::test(&server.uri()),
                },
            )
            .await
            .unwrap_err();
            assert!(
                second_error
                    .to_string()
                    .contains("OAuth login session is no longer active")
            );
            let token_polls = server
                .received_requests()
                .await
                .unwrap()
                .into_iter()
                .filter(|request| request.url.path() == "/login/oauth/access_token")
                .count();
            assert_eq!(token_polls, 1);
        }
    }

    #[tokio::test]
    async fn durable_session_survives_restart_and_ready_result_is_single_use() {
        let server = MockServer::start().await;
        mount_start(&server).await;
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "github-raw-secret",
                "token_type": "bearer",
                "scope": "workflow,repo"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/user"))
            .and(header("authorization", "Bearer github-raw-secret"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"id": 12345, "login": "octocat"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/copilot_internal/v2/token"))
            .and(header("authorization", "Bearer github-raw-secret"))
            .and(header("x-github-api-version", GITHUB_API_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "token": "short-copilot-token",
                "expires_at": 1_700_001_800,
                "refresh_in": 900,
                "endpoints": {"api": "https://copilot-api.github.com"}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (_directory, url, database) = sqlite_database().await;
        let started = start_at(
            &database,
            &reqwest::Client::new(),
            input(),
            KEY,
            NOW,
            true,
            &Endpoints::test(&server.uri()),
        )
        .await
        .unwrap();
        drop(database);
        let restarted = Database::connect(&url).await.unwrap();
        let result = poll_at(
            &restarted,
            &reqwest::Client::new(),
            &started.session_token,
            PollRuntime {
                key_material: KEY,
                now: NOW + 1_000,
                scope: CopilotDevicePollScope {
                    required_tenant: Some("tenant-a"),
                    operator_service_id: None,
                },
                allow_test_loopback: true,
                endpoints: &Endpoints::test(&server.uri()),
            },
        )
        .await
        .unwrap();
        let CopilotDevicePollResult::Ready { lease_owner, login } = result else {
            panic!("expected ready")
        };
        assert_eq!(login.stable_account_id, "github.com:12345");
        assert_eq!(login.scope, "repo workflow");
        assert_eq!(login.api_endpoint, "https://copilot-api.github.com");
        assert_eq!(
            login.provider_config["base_url"],
            "https://copilot-api.github.com"
        );
        let UpstreamCredential::OAuth {
            access_token,
            adapter_state: Some(state),
            ..
        } = &login.credential
        else {
            panic!("expected OAuth credential")
        };
        assert_eq!(access_token, "short-copilot-token");
        assert_eq!(state["github_token"], "github-raw-secret");
        assert_eq!(state["login"], "octocat");
        let account = restarted
            .create_upstream_account(
                CreateUpstreamAccountInput {
                    tenant_external_id: login.tenant_external_id.clone(),
                    name: login.account_name.clone(),
                    driver: PROVIDER_DRIVER.to_owned(),
                    config: login.provider_config.clone(),
                    credential: login.credential.clone(),
                    oauth_session_id: Some(login.session_id),
                    oauth_driver: Some(OAUTH_DRIVER.to_owned()),
                    oauth_refresh_url: Some(COPILOT_TOKEN_ENDPOINT.to_owned()),
                },
                KEY,
            )
            .await
            .unwrap();
        restarted
            .finish_oauth_login_session(login.session_id, lease_owner, account.id, NOW + 2_000)
            .await
            .unwrap();
        let consumed = poll_at(
            &restarted,
            &reqwest::Client::new(),
            &started.session_token,
            PollRuntime {
                key_material: KEY,
                now: NOW + 2_000,
                scope: CopilotDevicePollScope {
                    required_tenant: Some("tenant-a"),
                    operator_service_id: None,
                },
                allow_test_loopback: true,
                endpoints: &Endpoints::test(&server.uri()),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            consumed,
            CopilotDevicePollResult::Consumed {
                account_id: consumed_id,
                ..
            } if consumed_id == account.id
        ));
    }

    #[tokio::test]
    async fn exchange_accepts_minimal_and_full_responses() {
        for (body, expected_api, expected_refresh) in [
            (
                json!({"token": "short-one", "expires_at": 1_700_001_800}),
                DEFAULT_COPILOT_API_ENDPOINT,
                DEFAULT_REFRESH_SECONDS,
            ),
            (
                json!({
                    "token": "short-two",
                    "expires_at": 1_700_001_800_000_i64,
                    "refresh_in": 777,
                    "endpoints": {"api": "https://copilot-proxy.example"}
                }),
                "https://copilot-proxy.example",
                777,
            ),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/copilot_internal/v2/token"))
                .and(header("authorization", "Bearer raw-token"))
                .and(header("x-github-api-version", GITHUB_API_VERSION))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
            let token = exchange_copilot(
                &reqwest::Client::new(),
                "raw-token",
                NOW,
                true,
                &Endpoints::test(&server.uri()),
            )
            .await
            .unwrap();
            assert_eq!(token.api_endpoint, expected_api);
            assert_eq!(token.refresh_in, expected_refresh);
            assert_eq!(token.expires_at, NOW + 1_800_000);
        }
    }

    #[tokio::test]
    async fn exchange_rejects_malformed_unauthorized_and_rate_limited_without_leaking_body() {
        for (status, body) in [
            (200, json!({"token": "", "expires_at": 1_700_001_800})),
            (401, json!({"message": "secret-401-body"})),
            (429, json!({"message": "secret-429-body"})),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/copilot_internal/v2/token"))
                .respond_with(ResponseTemplate::new(status).set_body_json(body))
                .mount(&server)
                .await;
            let error = exchange_copilot(
                &reqwest::Client::new(),
                "raw-super-secret",
                NOW,
                true,
                &Endpoints::test(&server.uri()),
            )
            .await
            .unwrap_err();
            let text = error.to_string();
            assert!(text.contains("GitHub Copilot authorization failed"));
            assert!(!text.contains("secret"));
            assert!(!text.contains("raw-super-secret"));
        }
    }

    #[tokio::test]
    async fn refresh_is_one_exchange_and_preserves_raw_github_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/copilot_internal/v2/token"))
            .and(header("authorization", "Bearer github-raw-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "token": "new-short-token",
                "expires_at": 1_700_001_800,
                "refresh_in": 1234,
                "endpoints": {"api": "https://new-api.example"}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let refreshed = refresh_at(
            &reqwest::Client::new(),
            &credential(oauth_state("github.com:12345")),
            NOW,
            true,
            &Endpoints::test(&server.uri()),
        )
        .await
        .unwrap();
        let UpstreamCredential::OAuth {
            access_token,
            refresh_token,
            adapter_state: Some(state),
            ..
        } = refreshed
        else {
            panic!("expected OAuth")
        };
        assert_eq!(access_token, "new-short-token");
        assert!(refresh_token.is_none());
        assert_eq!(state["github_token"], "github-raw-secret");
        assert_eq!(state["refresh_in"], 1234);
        assert_eq!(state["copilot_api_endpoint"], "https://new-api.example");
    }

    #[tokio::test]
    async fn refresh_rejects_identity_mismatch_before_network_and_redacts_debug() {
        let server = MockServer::start().await;
        let bad = credential(oauth_state("github.com:99999"));
        let error = refresh_at(
            &reqwest::Client::new(),
            &bad,
            NOW,
            true,
            &Endpoints::test(&server.uri()),
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("invalid GitHub Copilot OAuth state")
        );
        assert!(!format!("{bad:?}").contains("github-raw-secret"));
        assert_eq!(server.received_requests().await.unwrap().len(), 0);
    }

    #[test]
    fn account_id_requires_valid_encrypted_adapter_state_data() {
        assert_eq!(
            copilot_account_id(&credential(oauth_state("github.com:12345"))).unwrap(),
            "github.com:12345"
        );
        assert!(copilot_account_id(&credential(oauth_state("github.com:9"))).is_err());
        assert!(
            copilot_account_id(&UpstreamCredential::OAuth {
                access_token: "short".into(),
                refresh_token: None,
                expires_at: Some(NOW + 1_000),
                header: "authorization".into(),
                prefix: "Bearer ".into(),
                adapter_state: None,
            })
            .is_err()
        );
        let mut malformed = oauth_state("github.com:12345");
        malformed["github_user_id"] = json!("not-a-number");
        assert!(copilot_account_id(&credential(malformed)).is_err());
    }
}
