use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::HeaderValue;
use serde::Deserialize;
use serde_json::Value;

#[cfg(test)]
use crate::network::OutboundScope;
use crate::{error::AppError, network, provider::UpstreamCredential};

pub const TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
/// Fixed native Codex catalog protocol profile. This is independent from the
/// MemeLoop package version and is shared by health and catalog discovery.
pub(crate) const CLIENT_VERSION: &str = "0.146.0";
pub(crate) const ORIGINATOR: &str = "codex-tui";
pub(crate) const USER_AGENT: &str =
    "codex-tui/0.146.0 (Mac OS 26.5.0; arm64) iTerm.app/3.6.10 (codex-tui; 0.146.0)";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const NATIVE_ADAPTER_SCHEMA: &str = "openai-codex-oauth-v1";
const RESPONSE_LIMIT: usize = 1024 * 1024;
const MAX_EXPIRES_IN_SECONDS: i64 = 365 * 24 * 60 * 60;
#[cfg(not(test))]
const REFRESH_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const REFRESH_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
}

pub async fn refresh(
    http: &reqwest::Client,
    credential: &UpstreamCredential,
    allow_test_loopback: bool,
) -> Result<UpstreamCredential, AppError> {
    refresh_at(http, credential, allow_test_loopback, TOKEN_ENDPOINT).await
}

async fn refresh_at(
    http: &reqwest::Client,
    credential: &UpstreamCredential,
    allow_test_loopback: bool,
    endpoint: &str,
) -> Result<UpstreamCredential, AppError> {
    let UpstreamCredential::OAuth {
        refresh_token: Some(refresh_token),
        adapter_state,
        ..
    } = credential
    else {
        return Err(AppError::BadRequest(
            "OpenAI Codex OAuth credential has no refresh token".into(),
        ));
    };
    super::required_secret(refresh_token, "OpenAI Codex")
        .map_err(|_| AppError::BadRequest("OpenAI Codex OAuth credential is invalid".into()))?;
    validate_adapter_state(adapter_state.as_ref())?;

    let client = network::client_for_codex_oauth_url(
        http,
        endpoint,
        credential.proxy(),
        allow_test_loopback,
    )
    .await
    .map_err(|_| refresh_failed())?;
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "refresh_token")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("refresh_token", refresh_token)
        .append_pair("scope", "openid profile email")
        .finish();
    let operation = async {
        let response = client
            .post(endpoint)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form)
            .timeout(REFRESH_TIMEOUT)
            .send()
            .await
            .map_err(|_| refresh_failed())?;
        if !response.status().is_success() {
            return Err(refresh_failed());
        }
        bounded_body(response).await
    };
    let body = tokio::time::timeout(REFRESH_TIMEOUT, operation)
        .await
        .map_err(|_| refresh_failed())??;
    let response: TokenResponse = serde_json::from_slice(&body).map_err(|_| refresh_failed())?;
    super::bearer_token(&response.access_token, "OpenAI Codex").map_err(|_| refresh_failed())?;
    if response.refresh_token.as_deref().is_some_and(str::is_empty) {
        return Err(refresh_failed());
    }
    super::optional_secret(response.refresh_token.as_deref(), "OpenAI Codex")
        .map_err(|_| refresh_failed())?;
    if !(1..=MAX_EXPIRES_IN_SECONDS).contains(&response.expires_in) {
        return Err(refresh_failed());
    }
    let expires_at = crate::db::unix_millis()
        .checked_add(
            response
                .expires_in
                .checked_mul(1_000)
                .ok_or_else(refresh_failed)?,
        )
        .ok_or_else(refresh_failed)?;

    Ok(UpstreamCredential::OAuth {
        access_token: response.access_token,
        refresh_token: response
            .refresh_token
            .or_else(|| Some(refresh_token.to_owned())),
        expires_at: Some(expires_at),
        header: "authorization".to_owned(),
        prefix: "Bearer ".to_owned(),
        adapter_state: adapter_state.clone(),
        proxy_url: credential.proxy().map(|(url, _)| url.to_owned()),
        proxy_network_scope: credential.proxy().map(|(_, scope)| scope),
    })
}

fn validate_adapter_state(state: Option<&Value>) -> Result<(), AppError> {
    let Some(state) = state else {
        return Err(AppError::BadRequest(
            "OpenAI Codex OAuth credential has invalid adapter state".into(),
        ));
    };
    let Some(object) = state.as_object() else {
        return Err(AppError::BadRequest(
            "OpenAI Codex OAuth credential has invalid adapter state".into(),
        ));
    };
    if object.len() != 2
        || !matches!(
            object.get("schema").and_then(Value::as_str),
            Some(NATIVE_ADAPTER_SCHEMA)
        )
    {
        return Err(AppError::BadRequest(
            "OpenAI Codex OAuth credential has invalid adapter state".into(),
        ));
    }
    let account_id = object
        .get("account_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AppError::BadRequest("OpenAI Codex OAuth credential has invalid adapter state".into())
        })?;
    super::account_id(account_id, "OpenAI Codex").map_err(|_| {
        AppError::BadRequest("OpenAI Codex OAuth credential has invalid adapter state".into())
    })
}

/// Return the OpenAI account identity used by the audited Codex wire protocol.
/// This accessor is crate-private so decrypted adapter state cannot escape
/// through provider views, logs, or public response types.
pub(crate) fn account_header_value(
    credential: &UpstreamCredential,
) -> Result<HeaderValue, AppError> {
    let UpstreamCredential::OAuth { adapter_state, .. } = credential else {
        return Err(invalid_adapter_state());
    };
    validate_adapter_state(adapter_state.as_ref())?;
    let account_id = adapter_state
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|state| state.get("account_id"))
        .and_then(Value::as_str)
        .ok_or_else(invalid_adapter_state)?;
    if !(1..=super::MAX_ACCOUNT_ID_BYTES).contains(&account_id.len())
        || !account_id.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    {
        return Err(invalid_adapter_state());
    }
    HeaderValue::from_str(account_id).map_err(|_| invalid_adapter_state())
}

fn invalid_adapter_state() -> AppError {
    AppError::BadRequest("OpenAI Codex OAuth credential has invalid adapter state".into())
}

async fn bounded_body(response: reqwest::Response) -> Result<Vec<u8>, AppError> {
    if response
        .content_length()
        .is_some_and(|length| length > RESPONSE_LIMIT as u64)
    {
        return Err(refresh_failed());
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| refresh_failed())?;
        if body.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
            return Err(refresh_failed());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn refresh_failed() -> AppError {
    AppError::Upstream("OpenAI Codex OAuth refresh failed".into())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_string_contains, header, method, path},
    };

    use super::*;

    fn credential(refresh_token: &str) -> UpstreamCredential {
        credential_with_schema(refresh_token, NATIVE_ADAPTER_SCHEMA)
    }

    fn credential_with_schema(refresh_token: &str, schema: &str) -> UpstreamCredential {
        UpstreamCredential::OAuth {
            access_token: "old-access-secret".to_owned(),
            refresh_token: Some(refresh_token.to_owned()),
            expires_at: Some(1),
            header: "authorization".to_owned(),
            prefix: "Bearer ".to_owned(),
            adapter_state: Some(json!({
                "schema": schema,
                "account_id": "account-123"
            })),
            proxy_url: None,
            proxy_network_scope: None,
        }
    }

    #[test]
    fn account_header_is_strict_and_never_falls_back_to_local_identity() {
        let valid = credential("refresh-secret");
        assert_eq!(account_header_value(&valid).unwrap(), "account-123");
        assert_eq!(
            account_header_value(&credential_with_schema(
                "refresh-secret",
                NATIVE_ADAPTER_SCHEMA
            ))
            .unwrap(),
            "account-123"
        );

        let rejected = [
            json!({"schema": "openai-codex-oauth-v1", "account_id": "account 123"}),
            json!({"schema": "openai-codex-oauth-v1", "account_id": "账户"}),
            json!({"schema": "openai-codex-oauth-v1", "account_id": "account-123", "extra": true}),
            json!({"schema": "wrong", "account_id": "account-123"}),
        ];
        for adapter_state in rejected {
            let mut candidate = credential("refresh-secret");
            if let UpstreamCredential::OAuth {
                adapter_state: state,
                ..
            } = &mut candidate
            {
                *state = Some(adapter_state);
            }
            let error = account_header_value(&candidate).unwrap_err();
            assert_eq!(
                error.to_string(),
                "invalid request: OpenAI Codex OAuth credential has invalid adapter state"
            );
        }
    }

    #[tokio::test]
    async fn refresh_uses_fixed_form_and_rotates_tokens_without_changing_state() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(header("content-type", "application/x-www-form-urlencoded"))
            .and(header("accept", "application/json"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains(format!("client_id={CLIENT_ID}")))
            .and(body_string_contains("refresh_token=old-refresh-secret"))
            .and(body_string_contains("scope=openid+profile+email"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("connection", "close")
                    .set_body_json(json!({
                        "access_token": "new-access-secret",
                        "refresh_token": "new-refresh-secret",
                        "expires_in": 3600,
                        "token_type": "Bearer"
                    })),
            )
            .mount(&server)
            .await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = listener.local_addr().unwrap();
        let target_address = *server.address();
        let proxy = tokio::spawn(async move {
            let (mut client, _) = listener.accept().await.unwrap();
            let mut greeting = [0_u8; 2];
            client.read_exact(&mut greeting).await.unwrap();
            let mut methods = vec![0_u8; usize::from(greeting[1])];
            client.read_exact(&mut methods).await.unwrap();
            client.write_all(&[5, 0]).await.unwrap();

            let mut request = [0_u8; 4];
            client.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, &[5, 1, 0, 3]);
            let mut hostname_length = [0_u8; 1];
            client.read_exact(&mut hostname_length).await.unwrap();
            let mut hostname = vec![0_u8; usize::from(hostname_length[0])];
            client.read_exact(&mut hostname).await.unwrap();
            assert_eq!(hostname, b"codex-refresh.test");
            let mut port = [0_u8; 2];
            client.read_exact(&mut port).await.unwrap();
            assert_eq!(u16::from_be_bytes(port), target_address.port());
            let mut upstream = TcpStream::connect(target_address).await.unwrap();
            client
                .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
            tokio::io::copy_bidirectional(&mut client, &mut upstream)
                .await
                .unwrap();
        });
        let mut old_credential =
            credential_with_schema("old-refresh-secret", NATIVE_ADAPTER_SCHEMA);
        if let UpstreamCredential::OAuth {
            proxy_url,
            proxy_network_scope,
            ..
        } = &mut old_credential
        {
            *proxy_url = Some(format!("socks5h://{proxy_address}"));
            *proxy_network_scope = Some(OutboundScope::Private);
        }
        let before = crate::db::unix_millis();
        let refreshed = refresh_at(
            &crate::build_http_client().unwrap(),
            &old_credential,
            true,
            &format!(
                "http://codex-refresh.test:{}/oauth/token",
                server.address().port()
            ),
        )
        .await
        .unwrap();
        let rendered = serde_json::to_value(&refreshed).unwrap();
        assert_eq!(rendered["access_token"], "new-access-secret");
        assert_eq!(rendered["refresh_token"], "new-refresh-secret");
        assert_eq!(rendered["header"], "authorization");
        assert_eq!(rendered["prefix"], "Bearer ");
        assert_eq!(rendered["adapter_state"]["account_id"], "account-123");
        assert_eq!(rendered["adapter_state"]["schema"], NATIVE_ADAPTER_SCHEMA);
        assert_eq!(rendered["proxy_url"], format!("socks5h://{proxy_address}"));
        assert_eq!(rendered["proxy_network_scope"], "private");
        assert!(rendered["expires_at"].as_i64().unwrap() >= before + 3_600_000);

        let requests = server.received_requests().await.unwrap();
        let body = String::from_utf8_lossy(&requests[0].body);
        let pairs = url::form_urlencoded::parse(body.as_bytes()).collect::<Vec<_>>();
        assert!(pairs.len() == 4);
        assert!(
            ["grant_type", "client_id", "refresh_token", "scope"]
                .into_iter()
                .all(|name| pairs.iter().filter(|(key, _)| key == name).count() == 1)
        );
        assert!(!body.contains("client_secret"));
        assert!(!body.contains("old-access-secret"));
        tokio::time::timeout(Duration::from_secs(2), proxy)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn refresh_retains_old_refresh_token_when_rotation_is_omitted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "new-access-secret",
                "expires_in": 60
            })))
            .mount(&server)
            .await;
        let refreshed = refresh_at(
            &crate::build_http_client().unwrap(),
            &credential("old-refresh-secret"),
            true,
            &format!("{}/token", server.uri()),
        )
        .await
        .unwrap();
        assert_eq!(
            serde_json::to_value(refreshed).unwrap()["refresh_token"],
            "old-refresh-secret"
        );
    }

    #[tokio::test]
    async fn refresh_failures_are_bounded_timed_out_and_redacted() {
        let cases = [
            ResponseTemplate::new(401).set_body_string("response-body-secret"),
            ResponseTemplate::new(200).set_body_string("response-body-secret"),
            ResponseTemplate::new(200)
                .set_body_string("response-body-secret".repeat(RESPONSE_LIMIT / 20 + 2)),
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(500))
                .set_body_json(json!({
                    "access_token": "delayed-response-secret",
                    "expires_in": 60
                })),
        ];
        for response in cases {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/token"))
                .respond_with(response)
                .mount(&server)
                .await;
            let error = refresh_at(
                &crate::build_http_client().unwrap(),
                &credential("request-refresh-secret"),
                true,
                &format!("{}/token", server.uri()),
            )
            .await
            .unwrap_err();
            let rendered = format!("{error:?} {error}");
            assert_eq!(
                error.to_string(),
                "configured upstream is unavailable: OpenAI Codex OAuth refresh failed"
            );
            for secret in [
                "response-body-secret",
                "delayed-response-secret",
                "request-refresh-secret",
            ] {
                assert!(!rendered.contains(secret));
            }
            assert!(!rendered.contains(&server.uri()));
        }
    }
}
