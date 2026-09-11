//! Native Kimi OAuth refresh and request identity handling.
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{error::AppError, network, provider::UpstreamCredential};

pub const PROVIDER_DRIVER: &str = "kimi-oauth";
pub const BASE_URL: &str = "https://api.kimi.com/coding";
pub const TOKEN_ENDPOINT: &str = "https://auth.kimi.com/api/oauth/token";
const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const SCHEMA: &str = "kimi-oauth-v1";
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeImportDocument {
    #[serde(rename = "type")]
    kind: String,
    access_token: String,
    refresh_token: String,
    token_type: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    device_id: Option<String>,
    #[serde(default)]
    expired: Option<String>,
    #[serde(default)]
    last_refresh: Option<String>,
    #[serde(default)]
    disabled: bool,
}

fn invalid() -> AppError {
    AppError::BadRequest("Kimi OAuth credential is invalid".into())
}

fn failed() -> AppError {
    AppError::Upstream("Kimi OAuth refresh failed".into())
}

fn optional_text(value: Option<&str>) -> Result<(), AppError> {
    if let Some(value) = value {
        super::controlled_text(value, 2048, true, "Kimi")?;
    }
    Ok(())
}

/// Parse the fixed native Kimi import document without constructing a network
/// client, resolving DNS, refreshing, or contacting the provider.
pub(crate) fn credential_from_native_import(
    payload: &Value,
) -> Result<UpstreamCredential, AppError> {
    let source: NativeImportDocument =
        serde_json::from_value(payload.clone()).map_err(|_| invalid())?;
    if source.kind != "kimi"
        || source.disabled
        || !source.token_type.eq_ignore_ascii_case("bearer")
    {
        return Err(invalid());
    }
    super::bearer_token(&source.access_token, "Kimi")?;
    super::required_secret(&source.refresh_token, "Kimi")?;
    optional_text(source.scope.as_deref())?;
    optional_text(source.device_id.as_deref())?;
    if let Some(last_refresh) = source.last_refresh.as_deref().filter(|value| !value.is_empty()) {
        rfc3339_millis(last_refresh)?;
    }
    let expires_at = source
        .expired
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(rfc3339_millis)
        .transpose()?;
    let credential = UpstreamCredential::OAuth {
        access_token: source.access_token,
        refresh_token: Some(source.refresh_token),
        expires_at,
        header: "authorization".to_owned(),
        prefix: "Bearer ".to_owned(),
        adapter_state: Some(json!({
            "schema": SCHEMA,
            "device_id": source.device_id,
            "scope": source.scope,
            "token_type": source.token_type,
        })),
        proxy_url: None,
        proxy_network_scope: None,
    };
    validate_credential(&credential)?;
    Ok(credential)
}

pub(crate) fn native_import_config() -> Value {
    json!({
        "base_url": BASE_URL,
        "network_scope": "public",
        "reservation_token_bounds": {},
    })
}

fn rfc3339_millis(value: &str) -> Result<i64, AppError> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.timestamp_millis())
        .map_err(|_| invalid())
}

pub(crate) fn validate_credential(credential: &UpstreamCredential) -> Result<(), AppError> {
    let UpstreamCredential::OAuth {
        access_token,
        refresh_token: Some(refresh_token),
        header,
        prefix,
        adapter_state: Some(state),
        ..
    } = credential
    else {
        return Err(invalid());
    };
    credential.validate(i64::MIN).map_err(|_| invalid())?;
    super::bearer_token(access_token, "Kimi")?;
    super::required_secret(refresh_token, "Kimi")?;
    let object = state.as_object().ok_or_else(invalid)?;
    if header != "authorization"
        || prefix != "Bearer "
        || object.len() != 4
        || state["schema"].as_str() != Some(SCHEMA)
        || !state["token_type"]
            .as_str()
            .is_some_and(|v| v.eq_ignore_ascii_case("bearer"))
    {
        return Err(invalid());
    }
    for name in ["device_id", "scope"] {
        let value = object.get(name).ok_or_else(invalid)?;
        if !value.is_null() && !value.is_string() {
            return Err(invalid());
        }
        optional_text(value.as_str())?;
    }
    Ok(())
}

/// Account device identity stays inside the encrypted credential envelope.
/// Authorization itself is applied by the shared send-time credential path.
pub(crate) fn apply_headers(
    request: reqwest::RequestBuilder,
    credential: &UpstreamCredential,
) -> Result<reqwest::RequestBuilder, AppError> {
    validate_credential(credential)?;
    let device = credential
        .adapter_state()
        .and_then(|v| v["device_id"].as_str())
        .filter(|v| !v.is_empty())
        .unwrap_or("cli-proxy-api-device");
    Ok(request
        .header(
            "User-Agent",
            concat!("memeloop-token-center/", env!("CARGO_PKG_VERSION")),
        )
        .header("X-Msh-Platform", "MemeLoop")
        .header("X-Msh-Version", env!("CARGO_PKG_VERSION"))
        .header("X-Msh-Device-Name", "memeloop-token-center")
        .header("X-Msh-Device-Model", "Linux")
        .header("X-Msh-Device-Id", device))
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<f64>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    scope: Option<String>,
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
    validate_credential(credential)?;
    let UpstreamCredential::OAuth {
        refresh_token: Some(refresh_token),
        expires_at,
        adapter_state,
        ..
    } = credential
    else {
        return Err(invalid());
    };
    let client = network::client_for_config_url(
        http,
        endpoint,
        &json!({"network_scope": "public"}),
        credential.proxy(),
        allow_test_loopback,
    )
    .await
    .map_err(|_| failed())?;
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", CLIENT_ID)
        .append_pair("grant_type", "refresh_token")
        .append_pair("refresh_token", refresh_token)
        .finish();
    let operation = async {
        let response = apply_headers(client.post(endpoint), credential)?
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(form)
            .timeout(TIMEOUT)
            .send()
            .await
            .map_err(|_| failed())?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(failed());
        }
        super::super::bounded_body(response)
            .await
            .map_err(|_| failed())
    };
    let bytes = tokio::time::timeout(TIMEOUT, operation)
        .await
        .map_err(|_| failed())??;
    let response: TokenResponse = serde_json::from_slice(&bytes).map_err(|_| failed())?;
    super::bearer_token(&response.access_token, "Kimi").map_err(|_| failed())?;
    let refreshed_token = response.refresh_token.filter(|v| !v.is_empty());
    super::optional_secret(refreshed_token.as_deref(), "Kimi").map_err(|_| failed())?;
    let expiry = match response.expires_in {
        None | Some(0.0) => *expires_at,
        Some(seconds) if seconds.is_finite() && seconds > 0.0 && seconds <= 31_536_000.0 => Some(
            crate::db::unix_millis()
                .checked_add((seconds * 1000.0) as i64)
                .ok_or_else(failed)?,
        ),
        _ => return Err(failed()),
    };
    let mut state = adapter_state.clone().ok_or_else(failed)?;
    if let Some(scope) = response.scope {
        optional_text(Some(&scope)).map_err(|_| failed())?;
        state["scope"] = Value::String(scope);
    }
    if let Some(token_type) = response.token_type {
        if !token_type.eq_ignore_ascii_case("bearer") {
            return Err(failed());
        }
        state["token_type"] = Value::String(token_type);
    }
    Ok(UpstreamCredential::OAuth {
        access_token: response.access_token,
        refresh_token: Some(refreshed_token.unwrap_or_else(|| refresh_token.clone())),
        expires_at: expiry,
        header: "authorization".to_owned(),
        prefix: "Bearer ".to_owned(),
        adapter_state: Some(state),
        proxy_url: credential.proxy().map(|(v, _)| v.to_owned()),
        proxy_network_scope: credential.proxy().map(|(_, scope)| scope),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_string_contains, header, method, path},
    };

    fn credential(device_id: &str) -> UpstreamCredential {
        UpstreamCredential::OAuth {
            access_token: "fixture-access".to_owned(),
            refresh_token: Some("fixture-refresh".to_owned()),
            expires_at: None,
            header: "authorization".to_owned(),
            prefix: "Bearer ".to_owned(),
            adapter_state: Some(json!({
                "schema": SCHEMA,
                "device_id": device_id,
                "scope": "coding",
                "token_type": "Bearer"
            })),
            proxy_url: None,
            proxy_network_scope: None,
        }
    }

    fn native_document(expired: &str) -> Value {
        json!({
            "type": "kimi",
            "access_token": "native-fixture-access",
            "refresh_token": "native-fixture-refresh",
            "token_type": "Bearer",
            "scope": "coding",
            "device_id": "native-fixture-device",
            "expired": expired,
            "last_refresh": "2026-09-01T00:00:00Z",
            "disabled": false
        })
    }

    #[test]
    fn native_import_parsing_is_local_strict_and_preserves_expiry() {
        let document = native_document("2099-01-01T00:00:00Z");
        let credential = credential_from_native_import(&document).unwrap();
        assert_eq!(
            credential.adapter_state().unwrap()["device_id"],
            "native-fixture-device"
        );
        assert_eq!(
            credential.expires_at(),
            Some(
                chrono::DateTime::parse_from_rfc3339("2099-01-01T00:00:00Z")
                    .unwrap()
                    .timestamp_millis()
            )
        );
        assert_eq!(native_import_config()["base_url"], BASE_URL);

        let mut disabled = document.clone();
        disabled["disabled"] = json!(true);
        assert!(credential_from_native_import(&disabled).is_err());
        let mut unknown = document;
        unknown["unexpected"] = json!("rejected");
        assert!(credential_from_native_import(&unknown).is_err());
    }

    #[test]
    fn catalog_resolves_native_adapter_and_account_specific_headers() {
        let catalog = crate::provider::ProviderCatalog::builtins();
        let adapter = catalog
            .managed_oauth_adapter_for_driver(PROVIDER_DRIVER)
            .unwrap();
        assert_eq!(adapter.refresh_url(), TOKEN_ENDPOINT);
        assert!(!catalog.supports_direct_credential(PROVIDER_DRIVER, "oauth"));
        assert!(!catalog.supports_direct_credential(PROVIDER_DRIVER, "api_key"));
        for device in ["device-one", "device-two"] {
            let credential = credential(device);
            let request = apply_headers(reqwest::Client::new().get(BASE_URL), &credential)
                .unwrap()
                .build()
                .unwrap();
            assert_eq!(request.headers()["x-msh-device-id"], device);
            assert!(!request.headers().contains_key("authorization"));
        }
    }

    #[tokio::test]
    async fn refresh_keeps_device_and_rotates_only_returned_fields() {
        for rotate in [false, true] {
            let server = MockServer::start().await;
            let mut response = json!({"access_token": "next-access", "expires_in": 3600.5});
            if rotate {
                response["refresh_token"] = json!("next-refresh");
                response["scope"] = json!("coding-next");
            }
            Mock::given(method("POST"))
                .and(path("/token"))
                .and(header("x-msh-device-id", "fixture-device"))
                .and(header("content-type", "application/x-www-form-urlencoded"))
                .and(body_string_contains(format!("client_id={CLIENT_ID}")))
                .and(body_string_contains("grant_type=refresh_token"))
                .and(body_string_contains("refresh_token=fixture-refresh"))
                .respond_with(ResponseTemplate::new(200).set_body_json(response))
                .expect(1)
                .mount(&server)
                .await;
            let credential = credential("fixture-device");
            let result = refresh_at(
                &crate::build_http_client().unwrap(),
                &credential,
                true,
                &format!("{}/token", server.uri()),
            )
            .await
            .unwrap();
            validate_credential(&result).unwrap();
            let value = serde_json::to_value(result).unwrap();
            assert_eq!(value["access_token"], "next-access");
            assert_eq!(
                value["refresh_token"],
                if rotate {
                    "next-refresh"
                } else {
                    "fixture-refresh"
                }
            );
            assert_eq!(
                value["adapter_state"]["scope"],
                if rotate { "coding-next" } else { "coding" }
            );
            assert_eq!(value["adapter_state"]["device_id"], "fixture-device");
            assert!(value["expires_at"].as_i64().unwrap() > crate::db::unix_millis());
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn refresh_failures_do_not_echo_remote_secrets() {
        for (status, response) in [
            (401, json!({"error": "fixture-refresh"})),
            (200, json!({"access_token": "", "expires_in": 3600})),
            (
                200,
                json!({"access_token": "remote-secret", "expires_in": -1}),
            ),
            (
                200,
                json!({"access_token": "remote-secret", "token_type": "Basic"}),
            ),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status).set_body_json(response))
                .mount(&server)
                .await;
            let credential = credential("fixture-device");
            let error = refresh_at(
                &crate::build_http_client().unwrap(),
                &credential,
                true,
                &format!("{}/token", server.uri()),
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(!error.contains("fixture-refresh"));
            assert!(!error.contains("remote-secret"));
        }
    }
}
