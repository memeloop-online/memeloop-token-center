//! Native CPA Kimi token import. Wire contract: CLIProxyAPI
//! v7.2.128-onetwo.1 internal/auth/kimi/{token,kimi}.go.
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    error::AppError,
    network::{self, OutboundScope},
    oauth::ManagedOAuthNormalizedAccount,
    provider::UpstreamCredential,
};

pub const PROVIDER_DRIVER: &str = "kimi-oauth";
pub const BASE_URL: &str = "https://api.kimi.com/coding";
pub const TOKEN_ENDPOINT: &str = "https://auth.kimi.com/api/oauth/token";
const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const SCHEMA: &str = "kimi-oauth-v1";
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
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
    #[serde(default)]
    proxy_url: Option<String>,
}

fn invalid() -> AppError {
    AppError::BadRequest("CPA Kimi OAuth document is invalid".into())
}

fn failed() -> AppError {
    AppError::Upstream("Kimi OAuth refresh failed".into())
}

fn optional_text(value: Option<&str>) -> Result<(), AppError> {
    if let Some(value) = value {
        super::controlled_text(value, 2048, true, "CPA Kimi")?;
    }
    Ok(())
}

pub fn normalize(payload: &Value) -> Result<ManagedOAuthNormalizedAccount, AppError> {
    let source: Document = serde_json::from_value(payload.clone()).map_err(|_| invalid())?;
    if source.kind != "kimi" || !source.token_type.eq_ignore_ascii_case("bearer") {
        return Err(invalid());
    }
    super::bearer_token(&source.access_token, "CPA Kimi")?;
    super::required_secret(&source.refresh_token, "CPA Kimi")?;
    optional_text(source.scope.as_deref())?;
    optional_text(source.device_id.as_deref())?;
    if let Some(last_refresh) = source.last_refresh.as_deref().filter(|v| !v.is_empty()) {
        super::timestamp_millis(last_refresh, "CPA Kimi")?;
    }
    let expires_at = source
        .expired
        .as_deref()
        .filter(|v| !v.is_empty())
        .map(|v| super::timestamp_millis(v, "CPA Kimi"))
        .transpose()?;
    let proxy_url = source
        .proxy_url
        .map(|v| super::codex::normalize_private_proxy_url(&v).map_err(|_| invalid()))
        .transpose()?;
    let proxy_network_scope = proxy_url.as_ref().map(|_| OutboundScope::Private);
    Ok(ManagedOAuthNormalizedAccount {
        account_name: "Kimi account".to_owned(),
        config: json!({"base_url": BASE_URL, "network_scope": "public", "reservation_token_bounds": {}}),
        enabled: !source.disabled,
        credential: UpstreamCredential::OAuth {
            access_token: source.access_token,
            refresh_token: Some(source.refresh_token),
            expires_at,
            header: "authorization".to_owned(),
            prefix: "Bearer ".to_owned(),
            adapter_state: Some(json!({
                "schema": SCHEMA, "device_id": source.device_id,
                "scope": source.scope, "token_type": source.token_type,
            })),
            proxy_url,
            proxy_network_scope,
        },
    })
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
    super::bearer_token(access_token, "CPA Kimi")?;
    super::required_secret(refresh_token, "CPA Kimi")?;
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
    super::bearer_token(&response.access_token, "CPA Kimi").map_err(|_| failed())?;
    let refreshed_token = response.refresh_token.filter(|v| !v.is_empty());
    super::optional_secret(refreshed_token.as_deref(), "CPA Kimi").map_err(|_| failed())?;
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

    fn document() -> Value {
        json!({
            "type": "kimi", "access_token": "fixture-access",
            "refresh_token": "fixture-refresh", "token_type": "Bearer",
            "device_id": "fixture-device", "scope": "coding",
            "expired": "2099-01-01T00:00:00Z",
        })
    }

    #[test]
    fn import_preserves_oauth_identity_and_unknown_expiry() {
        let source = document();
        let account = normalize(&source).unwrap();
        validate_credential(&account.credential).unwrap();
        let value = serde_json::to_value(&account.credential).unwrap();
        assert_eq!(value["access_token"], source["access_token"]);
        assert_eq!(value["refresh_token"], source["refresh_token"]);
        assert_eq!(value["adapter_state"]["device_id"], source["device_id"]);
        assert_eq!(value["adapter_state"]["scope"], source["scope"]);
        assert_eq!(account.config["base_url"], BASE_URL);
        assert!(account.enabled);
        let mut no_expiry = source.clone();
        no_expiry.as_object_mut().unwrap().remove("expired");
        assert_eq!(
            serde_json::to_value(normalize(&no_expiry).unwrap().credential).unwrap()["expires_at"],
            Value::Null
        );
        no_expiry["disabled"] = json!(true);
        assert!(!normalize(&no_expiry).unwrap().enabled);
        let debug = format!("{:?}", account.credential);
        assert!(!debug.contains("fixture-access"));
        assert!(!debug.contains("fixture-refresh"));
        assert!(!debug.contains("fixture-device"));
    }

    #[test]
    fn import_rejects_unsupported_or_injected_state() {
        for (name, value) in [
            ("type", json!("codex")),
            ("token_type", json!("Basic")),
            ("device_id", json!("device\r\nAuthorization: injected")),
            ("expired", json!("not-a-date")),
            ("access_token", json!("a\nb")),
            ("base_url", json!("https://attacker.example")),
            ("proxy_url", json!("socks5h://attacker.example:1080")),
        ] {
            let mut source = document();
            source[name] = value;
            let error = normalize(&source).unwrap_err().to_string();
            assert!(!error.contains("fixture-access"));
            assert!(!error.contains("attacker"));
        }
        let mut source = document();
        source["proxy_url"] = json!("socks5h://user:pass@10.2.3.4:1080");
        let credential = normalize(&source).unwrap().credential;
        assert_eq!(
            credential.proxy(),
            Some(("socks5h://user:pass@10.2.3.4:1080", OutboundScope::Private))
        );
    }

    #[test]
    fn catalog_resolves_native_adapter_and_account_specific_headers() {
        let catalog = crate::provider::ProviderCatalog::builtins();
        let adapter = catalog.managed_oauth_adapter_for_source("kimi").unwrap();
        assert_eq!(adapter.provider_driver(), PROVIDER_DRIVER);
        assert!(adapter.normalize_url().is_none());
        assert_eq!(adapter.refresh_url(), TOKEN_ENDPOINT);
        assert!(adapter.can_refresh());
        assert!(!catalog.supports_direct_credential(PROVIDER_DRIVER, "oauth"));
        assert!(!catalog.supports_direct_credential(PROVIDER_DRIVER, "api_key"));
        for device in ["device-one", "device-two"] {
            let mut source = document();
            source["device_id"] = json!(device);
            let credential = normalize(&source).unwrap().credential;
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
            let credential = normalize(&document()).unwrap().credential;
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
            let credential = normalize(&document()).unwrap().credential;
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
