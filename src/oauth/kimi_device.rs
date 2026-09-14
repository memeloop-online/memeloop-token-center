//! Native Kimi RFC 8628 flow. Device codes and issued credentials stay in the
//! existing encrypted, leased OAuth session store; no supplier refresh here.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{OAuthReauthorizationTarget, managed::kimi};
use crate::{
    db::{BeginOAuthLoginSession, Database, OAuthLoginClaim, OAuthLoginSessionReference},
    error::AppError,
    network::{self, OutboundScope},
    provider::{UpstreamCredential, open_private_json, seal_private_json},
};

pub const DEVICE_ENDPOINT: &str = "https://auth.kimi.com/api/oauth/device_authorization";
pub const OAUTH_DRIVER: &str = "kimi-oauth";
const SESSION_AAD: &[u8] = b"mtc/kimi-device/session/v1";
const STATE_AAD: &[u8] = b"mtc/kimi-device/state/v1";
const READY_AAD: &[u8] = b"mtc/kimi-device/ready/v1";

pub struct StartKimiDeviceLogin {
    pub tenant_external_id: String,
    pub account_name: String,
    pub operator_service_id: Option<Uuid>,
    pub provider_config: Value,
    pub proxy_url: Option<String>,
    pub device_id: Option<String>,
    pub previous_scope: Option<String>,
    pub reauthorize: Option<OAuthReauthorizationTarget>,
}

#[derive(Serialize)]
pub struct KimiDeviceLoginStart {
    pub driver: &'static str,
    pub verification_url: String,
    pub user_code: String,
    pub session_token: String,
    pub expires_at: i64,
    pub poll_after_seconds: u64,
    pub security_notice: &'static str,
}

#[derive(Serialize, Deserialize)]
pub struct ReadyKimiDeviceLogin {
    pub session_id: Uuid,
    pub tenant_external_id: String,
    pub account_name: String,
    pub provider_config: Value,
    pub credential: UpstreamCredential,
    pub reauthorize: Option<OAuthReauthorizationTarget>,
}

pub enum KimiDevicePollResult {
    Pending {
        retry_after_seconds: u64,
    },
    Ready {
        lease_owner: Uuid,
        login: Box<ReadyKimiDeviceLogin>,
    },
    Consumed {
        account_id: Uuid,
        tenant_external_id: String,
    },
}

pub struct KimiDevicePollScope<'a> {
    pub required_tenant: Option<&'a str>,
    pub operator_service_id: Option<Uuid>,
}

#[derive(Serialize, Deserialize)]
struct Session {
    session_id: Uuid,
    tenant_external_id: String,
    operator_service_id: Option<Uuid>,
    expires_at: i64,
}

#[derive(Serialize, Deserialize)]
struct LoginState {
    session: Session,
    account_name: String,
    provider_config: Value,
    device_code: String,
    device_id: String,
    previous_scope: Option<String>,
    proxy_url: Option<String>,
    interval: u64,
    reauthorize: Option<OAuthReauthorizationTarget>,
}

#[derive(Deserialize)]
struct DeviceResponse {
    device_code: String,
    user_code: String,
    #[serde(default)]
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: String,
    expires_in: Option<u64>,
    interval: Option<u64>,
}

fn failed() -> AppError {
    AppError::Upstream("Kimi OAuth request failed".into())
}

fn encode_form(form: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form.iter().copied())
        .finish()
}

async fn post(
    http: &reqwest::Client,
    endpoint: &str,
    proxy: Option<&str>,
    device: &str,
    form: &[(&str, &str)],
    allow_test_loopback: bool,
) -> Result<Value, AppError> {
    let client = network::client_for_config_url(
        http,
        endpoint,
        &json!({"network_scope":"public"}),
        proxy.map(|url| (url, OutboundScope::Private)),
        allow_test_loopback,
    )
    .await
    .map_err(|_| failed())?;
    let response = kimi::apply_device_headers(client.post(endpoint), device)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(encode_form(form))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|_| failed())?;
    let status = response.status();
    let bytes = super::bounded_body(response).await.map_err(|_| failed())?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| failed())?;
    // RFC 8628 pending responses may be HTTP 400 or (on Kimi) HTTP 200.
    if !status.is_success() && status != reqwest::StatusCode::BAD_REQUEST {
        return Err(failed());
    }
    Ok(value)
}

pub async fn start_kimi_device_login(
    db: &Database,
    http: &reqwest::Client,
    input: StartKimiDeviceLogin,
    key: &[u8],
    now: i64,
    allow_test_loopback: bool,
) -> Result<KimiDeviceLoginStart, AppError> {
    start_at(
        db,
        http,
        input,
        key,
        now,
        allow_test_loopback,
        DEVICE_ENDPOINT,
    )
    .await
}

async fn start_at(
    db: &Database,
    http: &reqwest::Client,
    input: StartKimiDeviceLogin,
    key: &[u8],
    now: i64,
    allow_test_loopback: bool,
    endpoint: &str,
) -> Result<KimiDeviceLoginStart, AppError> {
    for text in [&input.tenant_external_id, &input.account_name] {
        super::managed::controlled_text(text.trim(), 160, false, "Kimi")?;
    }
    if let Some(proxy) = input.proxy_url.as_deref() {
        crate::provider::validate_oauth_remote_dns_proxy_url(proxy, allow_test_loopback)?;
    }
    let device_id = input
        .device_id
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    super::managed::controlled_text(&device_id, 2048, false, "Kimi")?;
    let response: DeviceResponse = serde_json::from_value(
        post(
            http,
            endpoint,
            input.proxy_url.as_deref(),
            &device_id,
            &[("client_id", kimi::CLIENT_ID)],
            allow_test_loopback,
        )
        .await?,
    )
    .map_err(|_| failed())?;
    super::managed::required_secret(&response.device_code, "Kimi")?;
    super::managed::controlled_text(&response.user_code, 160, false, "Kimi")?;
    let verification_url = if response.verification_uri_complete.is_empty() {
        response.verification_uri
    } else {
        response.verification_uri_complete
    };
    let verification = url::Url::parse(&verification_url).map_err(|_| failed())?;
    if verification.scheme() != "https"
        || !verification
            .host_str()
            .is_some_and(|host| host == "kimi.com" || host.ends_with(".kimi.com"))
        || !verification.username().is_empty()
        || verification.password().is_some()
        || verification.port_or_known_default() != Some(443)
    {
        return Err(failed());
    }
    let lifetime = response
        .expires_in
        .filter(|seconds| *seconds > 0)
        .unwrap_or(900)
        .min(900);
    let interval = response.interval.unwrap_or(5).max(5);
    if interval >= lifetime {
        return Err(failed());
    }
    let expires_at = now
        .checked_add((lifetime * 1000) as i64)
        .ok_or_else(failed)?;
    let session = Session {
        session_id: Uuid::now_v7(),
        tenant_external_id: input.tenant_external_id,
        operator_service_id: input.operator_service_id,
        expires_at,
    };
    let token = seal_private_json(&session, key, SESSION_AAD)?;
    let state = LoginState {
        session,
        account_name: input.account_name.trim().to_owned(),
        provider_config: input.provider_config,
        device_code: response.device_code,
        device_id,
        previous_scope: input.previous_scope,
        proxy_url: input.proxy_url,
        interval,
        reauthorize: input.reauthorize,
    };
    db.begin_oauth_login_session(BeginOAuthLoginSession {
        session_id: state.session.session_id,
        flow_kind: OAUTH_DRIVER.to_owned(),
        tenant_external_id: state.session.tenant_external_id.clone(),
        operator_service_id: state.session.operator_service_id,
        state_ciphertext: seal_private_json(&state, key, STATE_AAD)?,
        next_poll_at: now + (interval * 1000) as i64,
        expires_at,
    })
    .await?;
    Ok(KimiDeviceLoginStart {
        driver: kimi::PROVIDER_DRIVER,
        verification_url,
        user_code: response.user_code,
        session_token: token,
        expires_at,
        poll_after_seconds: interval,
        security_notice: "only_continue_if_you_started_this_login",
    })
}

pub async fn poll_kimi_device_login(
    db: &Database,
    http: &reqwest::Client,
    token: &str,
    key: &[u8],
    now: i64,
    scope: KimiDevicePollScope<'_>,
    allow_test_loopback: bool,
) -> Result<KimiDevicePollResult, AppError> {
    poll_at(
        db,
        http,
        token,
        key,
        now,
        scope,
        allow_test_loopback,
        kimi::TOKEN_ENDPOINT,
    )
    .await
}

async fn poll_at(
    db: &Database,
    http: &reqwest::Client,
    token: &str,
    key: &[u8],
    now: i64,
    scope: KimiDevicePollScope<'_>,
    allow_test_loopback: bool,
    endpoint: &str,
) -> Result<KimiDevicePollResult, AppError> {
    poll_at_with_clock(
        db,
        http,
        token,
        key,
        now,
        scope,
        allow_test_loopback,
        endpoint,
        &crate::db::unix_millis,
    )
    .await
}

async fn poll_at_with_clock(
    db: &Database,
    http: &reqwest::Client,
    token: &str,
    key: &[u8],
    now: i64,
    scope: KimiDevicePollScope<'_>,
    allow_test_loopback: bool,
    endpoint: &str,
    clock: &(dyn Fn() -> i64 + Sync),
) -> Result<KimiDevicePollResult, AppError> {
    let session: Session = open_private_json(token, key, SESSION_AAD)
        .map_err(|_| AppError::BadRequest("invalid OAuth session token".into()))?;
    if scope
        .required_tenant
        .is_some_and(|tenant| tenant != session.tenant_external_id)
        || scope.operator_service_id != session.operator_service_id
    {
        return Err(AppError::Forbidden);
    }
    if now >= session.expires_at {
        return Err(AppError::BadRequest("OAuth login session expired".into()));
    }
    let reference = OAuthLoginSessionReference {
        session_id: session.session_id,
        flow_kind: OAUTH_DRIVER.to_owned(),
        tenant_external_id: session.tenant_external_id.clone(),
        operator_service_id: session.operator_service_id,
        expires_at: session.expires_at,
    };
    let (owner, encrypted) = match db.claim_oauth_login_poll(&reference, now, 5).await? {
        OAuthLoginClaim::Claimed {
            lease_owner,
            state_ciphertext,
        } => (lease_owner, state_ciphertext),
        other => return claim_result(other, &session, key),
    };
    let mut state: LoginState = open_private_json(&encrypted, key, STATE_AAD)?;
    if state.session.session_id != session.session_id
        || state.session.tenant_external_id != session.tenant_external_id
        || state.session.operator_service_id != session.operator_service_id
        || state.session.expires_at != session.expires_at
    {
        return Err(AppError::Forbidden);
    }
    let result = post(
        http,
        endpoint,
        state.proxy_url.as_deref(),
        &state.device_id,
        &[
            ("client_id", kimi::CLIENT_ID),
            ("device_code", &state.device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ],
        allow_test_loopback,
    )
    .await;
    // The supplier call can outlive the remaining session lifetime. Use its
    // completion time for expiry, token lifetime, lease updates and backoff.
    let now = clock().max(now);
    if now >= session.expires_at {
        db.fail_oauth_login_poll(session.session_id, owner, now)
            .await?;
        return Err(AppError::BadRequest("Kimi authorization expired".into()));
    }
    let response = match result {
        Ok(value) => value,
        Err(error) => {
            db.fail_oauth_login_poll(session.session_id, owner, now)
                .await?;
            return Err(error);
        }
    };
    match response["error"].as_str().filter(|value| !value.is_empty()) {
        Some(error @ ("authorization_pending" | "slow_down")) => {
            if error == "slow_down" {
                state.interval = state.interval.saturating_add(5);
            }
            let next = now
                .checked_add(
                    i64::try_from(state.interval.saturating_mul(1000)).map_err(|_| failed())?,
                )
                .ok_or_else(failed)?;
            if next >= session.expires_at {
                db.fail_oauth_login_poll(session.session_id, owner, now)
                    .await?;
                return Err(AppError::BadRequest("Kimi authorization expired".into()));
            }
            db.reschedule_oauth_login_poll(
                session.session_id,
                owner,
                seal_private_json(&state, key, STATE_AAD)?,
                next,
                now,
            )
            .await?;
            return Ok(KimiDevicePollResult::Pending {
                retry_after_seconds: state.interval,
            });
        }
        Some(_) => {
            db.fail_oauth_login_poll(session.session_id, owner, now)
                .await?;
            return Err(failed());
        }
        None => {}
    }
    let credential = match issued_credential(&response, &state, now) {
        Ok(value) => value,
        Err(error) => {
            db.fail_oauth_login_poll(session.session_id, owner, now)
                .await?;
            return Err(error);
        }
    };
    let ready = ReadyKimiDeviceLogin {
        session_id: session.session_id,
        tenant_external_id: session.tenant_external_id.clone(),
        account_name: state.account_name,
        provider_config: state.provider_config,
        credential,
        reauthorize: state.reauthorize,
    };
    db.stage_oauth_login_ready(
        session.session_id,
        owner,
        seal_private_json(&ready, key, READY_AAD)?,
        now,
    )
    .await?;
    claim_result(
        db.claim_oauth_login_poll(&reference, now, 5).await?,
        &session,
        key,
    )
}

fn claim_result(
    claim: OAuthLoginClaim,
    session: &Session,
    key: &[u8],
) -> Result<KimiDevicePollResult, AppError> {
    Ok(match claim {
        OAuthLoginClaim::Pending {
            retry_after_seconds,
        } => KimiDevicePollResult::Pending {
            retry_after_seconds,
        },
        OAuthLoginClaim::Consumed { account_id } => KimiDevicePollResult::Consumed {
            account_id,
            tenant_external_id: session.tenant_external_id.clone(),
        },
        OAuthLoginClaim::Ready {
            lease_owner,
            ready_ciphertext,
        } => KimiDevicePollResult::Ready {
            lease_owner,
            login: Box::new(open_private_json(&ready_ciphertext, key, READY_AAD)?),
        },
        OAuthLoginClaim::Claimed { .. } => return Err(AppError::Internal),
    })
}

fn issued_credential(
    response: &Value,
    state: &LoginState,
    now: i64,
) -> Result<UpstreamCredential, AppError> {
    let seconds = response["expires_in"]
        .as_f64()
        .filter(|value| value.is_finite() && *value > 0.0 && *value <= 31_536_000.0)
        .ok_or_else(failed)?;
    let expires_at = now
        .checked_add((seconds * 1000.0) as i64)
        .filter(|at| *at > now)
        .ok_or_else(failed)?;
    let expired = chrono::DateTime::from_timestamp_millis(expires_at)
        .ok_or_else(failed)?
        .to_rfc3339();
    // Reuse the native credential parser/schema and refresh metadata. Device ID
    // is a transport identity, not proof of the user's Kimi account identity.
    kimi::credential_from_native_import(&json!({
        "type":"kimi", "access_token":response["access_token"],
        "refresh_token":response["refresh_token"], "token_type":response["token_type"],
        "scope":response.get("scope").cloned().unwrap_or_else(|| json!(state.previous_scope)), "device_id":state.device_id,
        "expired":expired, "proxy_url":state.proxy_url,
    }))
    .map_err(|_| failed())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_string_contains, header, method, path},
    };

    #[test]
    fn form_post_future_is_send_without_retaining_the_form_serializer() {
        fn assert_send<T: Send>(_: T) {}
        let http = crate::build_no_retry_http_client(None, &[]).unwrap();
        assert_send(post(
            &http,
            DEVICE_ENDPOINT,
            None,
            "fixture-device",
            &[("client_id", kimi::CLIENT_ID)],
            false,
        ));
    }

    #[tokio::test]
    async fn slow_poll_uses_response_clock_and_cannot_stage_after_expiry() {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("clock.db").display()
        ))
        .await
        .unwrap();
        db.migrate().await.unwrap();
        let server = MockServer::start().await;
        let http = crate::build_no_retry_http_client(None, &[]).unwrap();
        let now = crate::db::unix_millis() + 3_600_000;
        let key = b"kimi-device-clock-fixture-key";
        Mock::given(method("POST"))
            .and(path("/device"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "device_code":"clock-device-code", "user_code":"CLOCK-CODE",
                "verification_uri":"https://auth.kimi.com/device", "expires_in":900, "interval":5
            })))
            .expect(1)
            .mount(&server)
            .await;
        let started = start_at(
            &db,
            &http,
            StartKimiDeviceLogin {
                tenant_external_id: "default".into(),
                account_name: "Kimi clock".into(),
                operator_service_id: None,
                provider_config: kimi::native_import_config(),
                proxy_url: None,
                device_id: None,
                previous_scope: None,
                reauthorize: None,
            },
            key,
            now,
            true,
            &format!("{}/device", server.uri()),
        )
        .await
        .unwrap();
        server.verify().await;
        server.reset().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"error":"authorization_pending"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let token_url = format!("{}/token", server.uri());
        let scope = || KimiDevicePollScope {
            required_tenant: None,
            operator_service_id: None,
        };
        assert!(matches!(
            poll_at_with_clock(
                &db,
                &http,
                &started.session_token,
                key,
                now + 5000,
                scope(),
                true,
                &token_url,
                &|| now + 9000
            )
            .await
            .unwrap(),
            KimiDevicePollResult::Pending {
                retry_after_seconds: 5
            }
        ));
        // Backoff begins after the response, not from request admission.
        assert!(matches!(
            poll_at_with_clock(
                &db,
                &http,
                &started.session_token,
                key,
                now + 10000,
                scope(),
                true,
                &token_url,
                &|| now + 10000
            )
            .await
            .unwrap(),
            KimiDevicePollResult::Pending {
                retry_after_seconds: 4
            }
        ));
        server.verify().await;
        server.reset().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token":"late-access", "refresh_token":"late-refresh",
                "token_type":"bearer", "expires_in":3600
            })))
            .expect(1)
            .mount(&server)
            .await;
        assert!(
            matches!(poll_at_with_clock(&db, &http, &started.session_token, key,
            started.expires_at - 1, scope(), true, &token_url, &|| started.expires_at).await,
            Err(AppError::BadRequest(message)) if message == "Kimi authorization expired")
        );
        let session: Session = open_private_json(&started.session_token, key, SESSION_AAD).unwrap();
        let (status, ready): (String, Option<String>) = sqlx::query_as(
            "SELECT status, ready_ciphertext FROM oauth_login_sessions WHERE id = $1",
        )
        .bind(session.session_id.to_string())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(status, "failed");
        assert!(
            ready.is_none(),
            "late credentials must never become a ready result"
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn device_flow_leases_pending_slowdown_and_consumes_one_credential() {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("kimi.db").display()
        ))
        .await
        .unwrap();
        db.migrate().await.unwrap();
        let server = MockServer::start().await;
        let http = crate::build_no_retry_http_client(None, &[]).unwrap();
        let now = crate::db::unix_millis();
        let key = b"kimi-device-fixture-key";
        Mock::given(method("POST"))
            .and(path("/device"))
            .and(header("x-msh-device-id", "retained-device"))
            .and(body_string_contains(kimi::CLIENT_ID))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "device_code":"fixture-device-code", "user_code":"ABCD-EFGH",
                "verification_uri_complete":"https://www.kimi.com/device?user_code=ABCD-EFGH", "expires_in":900, "interval":5
            })))
            .expect(1)
            .mount(&server)
            .await;
        let started = start_at(
            &db,
            &http,
            StartKimiDeviceLogin {
                tenant_external_id: "default".into(),
                account_name: "Kimi native".into(),
                operator_service_id: None,
                provider_config: kimi::native_import_config(),
                proxy_url: None,
                device_id: Some("retained-device".into()),
                previous_scope: None,
                reauthorize: None,
            },
            key,
            now,
            true,
            &format!("{}/device", server.uri()),
        )
        .await
        .unwrap();
        assert!(!started.session_token.contains("fixture-device-code"));
        assert_eq!(
            started.verification_url,
            "https://www.kimi.com/device?user_code=ABCD-EFGH"
        );
        assert!(matches!(
            poll_at(
                &db,
                &http,
                &started.session_token,
                key,
                now,
                KimiDevicePollScope {
                    required_tenant: Some("other"),
                    operator_service_id: None
                },
                true,
                &format!("{}/token", server.uri())
            )
            .await,
            Err(AppError::Forbidden)
        ));
        assert!(matches!(
            poll_at(
                &db,
                &http,
                &started.session_token,
                key,
                now,
                KimiDevicePollScope {
                    required_tenant: None,
                    operator_service_id: None
                },
                true,
                &format!("{}/token", server.uri())
            )
            .await
            .unwrap(),
            KimiDevicePollResult::Pending { .. }
        ));
        server.verify().await;
        server.reset().await;
        for (offset, error, interval) in
            [(5000, "authorization_pending", 5), (10000, "slow_down", 10)]
        {
            Mock::given(method("POST"))
                .and(path("/token"))
                .and(header("x-msh-device-id", "retained-device"))
                .and(body_string_contains("device_code=fixture-device-code"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"error":error})))
                .expect(1)
                .mount(&server)
                .await;
            assert!(
                matches!(poll_at(&db, &http, &started.session_token, key, now + offset,
                KimiDevicePollScope { required_tenant:None, operator_service_id:None },
                true, &format!("{}/token", server.uri())).await.unwrap(),
                KimiDevicePollResult::Pending { retry_after_seconds } if retry_after_seconds == interval)
            );
            server.verify().await;
            server.reset().await;
        }
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(header("x-msh-device-id", "retained-device"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token":"fixture-access", "refresh_token":"fixture-refresh",
                "token_type":"bearer", "expires_in":3600, "scope":"coding"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let KimiDevicePollResult::Ready { lease_owner, login } = poll_at(
            &db,
            &http,
            &started.session_token,
            key,
            now + 20000,
            KimiDevicePollScope {
                required_tenant: None,
                operator_service_id: None,
            },
            true,
            &format!("{}/token", server.uri()),
        )
        .await
        .unwrap() else {
            panic!("expected ready")
        };
        assert_eq!(
            login.credential.adapter_state().unwrap()["device_id"],
            "retained-device"
        );
        assert_eq!(login.credential.adapter_state().unwrap()["scope"], "coding");
        let account = db
            .create_upstream_account(
                crate::db::CreateUpstreamAccountInput {
                    tenant_external_id: login.tenant_external_id,
                    name: login.account_name,
                    driver: kimi::PROVIDER_DRIVER.into(),
                    config: login.provider_config,
                    credential: login.credential,
                    oauth_session_id: Some(login.session_id),
                    oauth_driver: Some(OAUTH_DRIVER.into()),
                    oauth_refresh_url: Some(kimi::TOKEN_ENDPOINT.into()),
                },
                key,
            )
            .await
            .unwrap();
        assert!(account.can_reauthorize);
        db.finish_oauth_login_session(login.session_id, lease_owner, account.id, now + 20000)
            .await
            .unwrap();
        assert!(
            matches!(poll_at(&db, &http, &started.session_token, key, now + 21000,
            KimiDevicePollScope { required_tenant:None, operator_service_id:None },
            true, &format!("{}/token", server.uri())).await.unwrap(),
            KimiDevicePollResult::Consumed { account_id, .. } if account_id == account.id)
        );
        server.verify().await;
        server.reset().await;
        let (disconnected, _, _) = db
            .disconnect_upstream_oauth(account.id, "default", account.updated_at, key)
            .await
            .unwrap();
        let target = OAuthReauthorizationTarget {
            account_id: account.id,
            expected_updated_at: disconnected.updated_at,
            expected_credential_generation: disconnected.credential_generation,
        };
        assert!(
            db.upstream_oauth_reauthorization_proxy_snapshot(
                account.id,
                "default",
                target.expected_updated_at,
                target.expected_credential_generation,
                OAUTH_DRIVER,
                key
            )
            .await
            .unwrap()
            .is_none()
        );
        Mock::given(method("POST"))
            .and(path("/device"))
            .and(header("x-msh-device-id", "retained-device"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "device_code":"second-device-code", "user_code":"SECOND-CODE",
                "verification_uri":"https://auth.kimi.com/device", "expires_in":900, "interval":5
            })))
            .expect(1)
            .mount(&server)
            .await;
        let second = start_at(
            &db,
            &http,
            StartKimiDeviceLogin {
                tenant_external_id: "default".into(),
                account_name: account.name.clone(),
                operator_service_id: None,
                provider_config: account.config.clone(),
                proxy_url: None,
                device_id: Some("retained-device".into()),
                previous_scope: Some("coding".into()),
                reauthorize: Some(target),
            },
            key,
            now + 30000,
            true,
            &format!("{}/device", server.uri()),
        )
        .await
        .unwrap();
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("device_code=second-device-code"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token":"second-access", "refresh_token":"second-refresh",
                "token_type":"bearer", "expires_in":3600
            })))
            .expect(1)
            .mount(&server)
            .await;
        let KimiDevicePollResult::Ready { lease_owner, login } = poll_at(
            &db,
            &http,
            &second.session_token,
            key,
            now + 35000,
            KimiDevicePollScope {
                required_tenant: None,
                operator_service_id: None,
            },
            true,
            &format!("{}/token", server.uri()),
        )
        .await
        .unwrap() else {
            panic!("expected reauthorization ready")
        };
        let target = login.reauthorize.as_ref().unwrap();
        assert_eq!(login.credential.adapter_state().unwrap()["scope"], "coding");
        let input = crate::db::ReauthorizeUpstreamAccountInput {
            tenant_external_id: "default".into(),
            expected_updated_at: target.expected_updated_at,
            expected_credential_generation: target.expected_credential_generation,
            driver: kimi::PROVIDER_DRIVER.into(),
            oauth_session_id: login.session_id,
            oauth_driver: OAUTH_DRIVER.into(),
            oauth_refresh_url: Some(kimi::TOKEN_ENDPOINT.into()),
            provider_config: Some(login.provider_config.clone()),
            credential: login.credential.clone(),
        };
        let restored = db
            .reauthorize_upstream_account(account.id, input, key)
            .await
            .unwrap();
        assert_eq!(restored.id, account.id);
        assert_eq!(restored.name, account.name);
        assert_eq!(restored.config, account.config);
        assert_eq!(
            restored.credential_generation,
            account.credential_generation + 1
        );
        assert_eq!(restored.status, "active");
        db.finish_oauth_login_session(login.session_id, lease_owner, account.id, now + 35000)
            .await
            .unwrap();
        server.verify().await;
    }
}
