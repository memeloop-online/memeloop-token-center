use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::HeaderValue;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    error::AppError,
    network::{self, OutboundScope},
    oauth::ManagedOAuthNormalizedAccount,
    provider::UpstreamCredential,
};

pub const TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
const BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const NATIVE_ADAPTER_SCHEMA: &str = "openai-codex-oauth-v1";
/// The one historical envelope shape accepted exclusively by the controlled
/// account-upgrade operation. It is never accepted by the native transport or
/// refresh lifecycle.
const IMPORTED_ADAPTER_SCHEMA: &str = "cpa-codex-oauth-v1";
const RESPONSE_LIMIT: usize = 1024 * 1024;
const MAX_EXPIRES_IN_SECONDS: i64 = 365 * 24 * 60 * 60;
#[cfg(not(test))]
const REFRESH_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const REFRESH_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CodexDocument {
    #[serde(rename = "type")]
    provider_type: String,
    access_token: String,
    refresh_token: String,
    account_id: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    last_refresh: String,
    expired: String,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    proxy_url: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
}

pub fn normalize(payload: &Value) -> Result<ManagedOAuthNormalizedAccount, AppError> {
    let document: CodexDocument =
        serde_json::from_value(payload.clone()).map_err(|_| invalid_document())?;
    if document.provider_type != "codex" {
        return Err(invalid_document());
    }
    super::bearer_token(&document.access_token, "CPA Codex")?;
    super::required_secret(&document.refresh_token, "CPA Codex")?;
    super::optional_secret(document.id_token.as_deref(), "CPA Codex")?;
    super::account_id(&document.account_id, "CPA Codex")?;
    let account_name =
        super::account_name(document.email.as_deref(), "Codex account", "CPA Codex")?;
    let _ = super::timestamp_millis(&document.last_refresh, "CPA Codex")?;
    let expires_at = super::timestamp_millis(&document.expired, "CPA Codex")?;
    let proxy_url = document
        .proxy_url
        .as_deref()
        .map(normalize_private_proxy_url)
        .transpose()?;
    let proxy_network_scope = proxy_url.as_ref().map(|_| OutboundScope::Private);

    Ok(ManagedOAuthNormalizedAccount {
        account_name,
        config: json!({
            "base_url": BASE_URL,
            "network_scope": "public",
            // Empty is intentionally not a guessed default. An operator or a
            // reviewed model-metadata sync must populate the exact upstream
            // model limits before this account can carry traffic.
            "reservation_token_bounds": {},
        }),
        enabled: !document.disabled,
        credential: UpstreamCredential::OAuth {
            access_token: document.access_token,
            refresh_token: Some(document.refresh_token),
            expires_at: Some(expires_at),
            header: "authorization".to_owned(),
            prefix: "Bearer ".to_owned(),
            adapter_state: Some(json!({
                "schema": NATIVE_ADAPTER_SCHEMA,
                "account_id": document.account_id,
            })),
            proxy_url,
            proxy_network_scope,
        },
    })
}

/// Convert the credential envelope produced by the retired importer to the
/// native Codex ABI. Access/refresh tokens, expiry, proxy endpoint and proxy
/// authentication remain byte-for-byte represented. A safe IP-literal
/// `socks5://` proxy is restored to `socks5h://` because the retired importer
/// collapsed the source's remote-DNS scheme while storing the credential.
///
/// The database migration owns the transaction and CAS guards. Keeping the
/// conversion here makes it impossible for an API response or a caller-owned
/// JSON document to observe the plaintext credential while it is upgraded.
pub(crate) fn upgrade_imported_credential(
    credential: UpstreamCredential,
) -> Result<UpstreamCredential, AppError> {
    let UpstreamCredential::OAuth {
        access_token,
        refresh_token,
        expires_at,
        header,
        prefix,
        adapter_state,
        proxy_url,
        proxy_network_scope,
    } = credential
    else {
        return Err(AppError::BadRequest(
            "imported OpenAI Codex account has an invalid credential".into(),
        ));
    };
    let Some(mut state) = adapter_state else {
        return Err(AppError::BadRequest(
            "imported OpenAI Codex account has an invalid credential".into(),
        ));
    };
    let Some(object) = state.as_object_mut() else {
        return Err(AppError::BadRequest(
            "imported OpenAI Codex account has an invalid credential".into(),
        ));
    };
    if object.len() != 2
        || object.get("schema").and_then(Value::as_str) != Some(IMPORTED_ADAPTER_SCHEMA)
        || object
            .get("account_id")
            .and_then(Value::as_str)
            .is_none_or(|account_id| super::account_id(account_id, "OpenAI Codex").is_err())
    {
        return Err(AppError::BadRequest(
            "imported OpenAI Codex account has an invalid credential".into(),
        ));
    }
    object.insert(
        "schema".to_owned(),
        Value::String(NATIVE_ADAPTER_SCHEMA.to_owned()),
    );
    let upgraded = UpstreamCredential::OAuth {
        access_token,
        refresh_token,
        expires_at,
        header,
        prefix,
        adapter_state: Some(state),
        proxy_url,
        proxy_network_scope,
    };
    let (upgraded, _) = restore_remote_dns_proxy(upgraded)?;
    // `i64::MIN` validates the credential shape and encrypted transport
    // metadata without rejecting an intentionally disabled expired account.
    upgraded.validate(i64::MIN)?;
    validate_adapter_state(upgraded.adapter_state())?;
    Ok(upgraded)
}

pub(crate) fn restore_remote_dns_proxy(
    credential: UpstreamCredential,
) -> Result<(UpstreamCredential, bool), AppError> {
    let UpstreamCredential::OAuth {
        access_token,
        refresh_token,
        expires_at,
        header,
        prefix,
        adapter_state,
        proxy_url,
        proxy_network_scope,
    } = credential
    else {
        return Err(AppError::BadRequest(
            "OpenAI Codex account has an invalid credential".into(),
        ));
    };
    let restored_proxy_url = restore_imported_remote_dns_proxy(proxy_url.clone())?;
    let changed = restored_proxy_url != proxy_url;
    Ok((
        UpstreamCredential::OAuth {
            access_token,
            refresh_token,
            expires_at,
            header,
            prefix,
            adapter_state,
            proxy_url: restored_proxy_url,
            proxy_network_scope,
        },
        changed,
    ))
}

fn restore_imported_remote_dns_proxy(
    proxy_url: Option<String>,
) -> Result<Option<String>, AppError> {
    let Some(proxy_url) = proxy_url else {
        return Ok(None);
    };
    let parsed = url::Url::parse(&proxy_url).map_err(|_| invalid_document())?;
    if parsed.scheme() == "socks5h" {
        normalize_private_proxy_url(&proxy_url)?;
        return Ok(Some(proxy_url));
    }
    if parsed.scheme() != "socks5" || !network::has_safe_private_ip_literal_host(&parsed) {
        return Err(invalid_document());
    }
    let suffix = proxy_url
        .strip_prefix("socks5://")
        .ok_or_else(invalid_document)?;
    let restored = format!("socks5h://{suffix}");
    normalize_private_proxy_url(&restored)?;
    Ok(Some(restored))
}

pub(crate) fn validate_native_credential(credential: &UpstreamCredential) -> Result<(), AppError> {
    credential.validate(i64::MIN)?;
    validate_adapter_state(credential.adapter_state())
}

/// Preserve only the native fixed destination and trusted reservation bounds
/// when an imported account is upgraded. A legacy configuration cannot smuggle
/// arbitrary outbound settings into the native driver.
pub(crate) fn native_config_from_import(config: &Value) -> Result<Value, AppError> {
    let Some(object) = config.as_object() else {
        return Err(AppError::BadRequest(
            "imported OpenAI Codex account has an invalid configuration".into(),
        ));
    };
    if object.len() != 3
        || object.get("base_url").and_then(Value::as_str) != Some(BASE_URL)
        || object.get("network_scope").and_then(Value::as_str) != Some("public")
    {
        return Err(AppError::BadRequest(
            "imported OpenAI Codex account has an invalid configuration".into(),
        ));
    }
    let Some(bounds) = object
        .get("reservation_token_bounds")
        .and_then(Value::as_object)
    else {
        return Err(AppError::BadRequest(
            "imported OpenAI Codex account has an invalid configuration".into(),
        ));
    };
    if bounds.len() > 10_000
        || bounds.iter().any(|(model, bound)| {
            model.is_empty()
                || model.len() > 500
                || bound
                    .as_i64()
                    .is_none_or(|value| !(1..=1_000_000_000).contains(&value))
        })
    {
        return Err(AppError::BadRequest(
            "imported OpenAI Codex account has an invalid configuration".into(),
        ));
    }
    Ok(json!({
        "base_url": BASE_URL,
        "network_scope": "public",
        "reservation_token_bounds": bounds,
    }))
}

/// Preserve the operator-selected SOCKS DNS semantics. `socks5` may use a
/// private DNS name because MTC resolves and pins it locally. `socks5h` is
/// accepted only with a safe private IP-literal proxy so the connection-time
/// resolver is an explicit, reviewable operator trust boundary.
fn normalize_private_proxy_url(value: &str) -> Result<String, AppError> {
    if value.len() > 2_048 || value.trim() != value || value.bytes().any(|byte| byte < 0x20) {
        return Err(invalid_document());
    }
    let parsed = url::Url::parse(value).map_err(|_| invalid_document())?;
    if !matches!(parsed.scheme(), "socks5" | "socks5h")
        || parsed.host_str().is_none()
        || parsed.port().is_none_or(|port| port == 0)
        || (parsed.path() != "" && parsed.path() != "/")
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(invalid_document());
    }
    let host = parsed.host_str().ok_or_else(invalid_document)?;
    let unbracketed = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(address) = unbracketed.parse::<std::net::IpAddr>() {
        if !network::is_safe_private_upstream_ip(address) {
            return Err(invalid_document());
        }
    } else if parsed.scheme() == "socks5h" {
        return Err(invalid_document());
    }
    Ok(value.to_owned())
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

    let client = network::client_for_config_url(
        http,
        endpoint,
        &json!({"network_scope": "public"}),
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

fn invalid_document() -> AppError {
    super::invalid_document("CPA Codex")
}

fn refresh_failed() -> AppError {
    AppError::Upstream("OpenAI Codex OAuth refresh failed".into())
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, SocketAddr};

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

    fn document() -> Value {
        json!({
            "type": "codex",
            "access_token": "access-token-secret",
            "refresh_token": "refresh-token-secret",
            "account_id": "account-123",
            "email": "codex@example.test",
            "id_token": "id-token-secret",
            "last_refresh": "2026-08-18T01:02:03Z",
            "expired": "2099-01-01T00:00:00Z"
        })
    }

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
    fn normalizes_valid_disabled_and_expired_documents_without_identity_leakage() {
        let normalized = normalize(&document()).unwrap();
        assert!(normalized.enabled);
        assert_eq!(normalized.account_name, "codex@example.test");
        assert_eq!(normalized.config["base_url"], BASE_URL);
        assert_eq!(normalized.config["reservation_token_bounds"], json!({}));
        let serialized = serde_json::to_value(&normalized.credential).unwrap();
        assert_eq!(serialized["expires_at"], 4_070_908_800_000_i64);
        assert_eq!(serialized["header"], "authorization");
        assert_eq!(serialized["prefix"], "Bearer ");
        assert_eq!(
            serialized["adapter_state"],
            json!({"schema": "openai-codex-oauth-v1", "account_id": "account-123"})
        );
        let state = serialized["adapter_state"].to_string();
        assert!(!state.contains("codex@example.test"));
        assert!(!state.contains("id-token-secret"));
        let debug = format!("{normalized:?}");
        assert!(!debug.contains("access-token-secret"));
        assert!(!debug.contains("refresh-token-secret"));
        assert!(!debug.contains("id-token-secret"));

        let mut disabled = document();
        disabled["disabled"] = json!(true);
        assert!(!normalize(&disabled).unwrap().enabled);

        let mut expired = document();
        expired["expired"] = json!("2020-01-01T00:00:00Z");
        assert!(
            normalize(&expired)
                .unwrap()
                .credential
                .expires_at()
                .is_some_and(|value| value < crate::db::unix_millis())
        );

        let mut unnamed = document();
        unnamed.as_object_mut().unwrap().remove("email");
        assert_eq!(normalize(&unnamed).unwrap().account_name, "Codex account");
    }

    #[test]
    fn normalizes_only_reviewable_private_cpa_proxy_shapes() {
        let mut literal = document();
        literal["proxy_url"] = json!("socks5h://proxy-user:proxy-secret@100.64.0.16:1080");
        let normalized = normalize(&literal).unwrap();
        assert_eq!(
            normalized.credential.proxy(),
            Some((
                "socks5h://proxy-user:proxy-secret@100.64.0.16:1080",
                OutboundScope::Private
            ))
        );
        let debug = format!("{:?}", normalized.credential);
        assert!(!debug.contains("proxy-secret"));
        assert!(!debug.contains("100.64.0.16"));

        let mut private_dns = document();
        private_dns["proxy_url"] = json!("socks5://proxy.service.svc.cluster.local:1080");
        assert_eq!(
            normalize(&private_dns).unwrap().credential.proxy(),
            Some((
                "socks5://proxy.service.svc.cluster.local:1080",
                OutboundScope::Private
            ))
        );

        for value in [
            "socks5h://proxy.service.svc.cluster.local:1080",
            "socks5h://8.8.8.8:1080",
            "socks5://127.0.0.1:1080",
            "socks5://169.254.169.254:1080",
            "socks5://100.100.100.200:1080",
            "http://100.64.0.16:1080",
            "socks5://100.64.0.16",
            "socks5://100.64.0.16:1080/path",
            "socks5://100.64.0.16:1080?token=secret",
            "socks5://100.64.0.16:1080#secret",
            " socks5://100.64.0.16:1080",
            "socks5://100.64.0.16:1080\nnext",
        ] {
            let mut rejected = document();
            rejected["proxy_url"] = json!(value);
            let error = normalize(&rejected).unwrap_err();
            let rendered = format!("{error:?} {error}");
            assert_eq!(
                error.to_string(),
                "invalid request: CPA Codex OAuth document is invalid",
                "{value}"
            );
            assert!(!rendered.contains(value));
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

    #[test]
    fn controlled_upgrade_restores_remote_dns_without_exposing_private_proxy() {
        let imported = UpstreamCredential::OAuth {
            access_token: "access-secret".to_owned(),
            refresh_token: Some("refresh-secret".to_owned()),
            expires_at: Some(4_070_908_800_000),
            header: "authorization".to_owned(),
            prefix: "Bearer ".to_owned(),
            adapter_state: Some(json!({
                "schema": IMPORTED_ADAPTER_SCHEMA,
                "account_id": "account-123"
            })),
            proxy_url: Some("socks5://operator:secret@100.64.0.16:1080".to_owned()),
            proxy_network_scope: Some(OutboundScope::Private),
        };
        let upgraded = upgrade_imported_credential(imported).unwrap();
        assert_eq!(
            upgraded.adapter_state(),
            Some(&json!({"schema": NATIVE_ADAPTER_SCHEMA, "account_id": "account-123"}))
        );
        assert_eq!(
            upgraded.proxy(),
            Some((
                "socks5h://operator:secret@100.64.0.16:1080",
                OutboundScope::Private
            ))
        );
        assert!(!format!("{upgraded:?}").contains("operator:secret"));
        assert!(!format!("{upgraded:?}").contains("access-secret"));
    }

    #[test]
    fn controlled_upgrade_preserves_existing_remote_dns_and_rejects_hostname_proxy() {
        let build = |proxy_url: &str| UpstreamCredential::OAuth {
            access_token: "access-secret".to_owned(),
            refresh_token: Some("refresh-secret".to_owned()),
            expires_at: Some(4_070_908_800_000),
            header: "authorization".to_owned(),
            prefix: "Bearer ".to_owned(),
            adapter_state: Some(json!({
                "schema": IMPORTED_ADAPTER_SCHEMA,
                "account_id": "account-123"
            })),
            proxy_url: Some(proxy_url.to_owned()),
            proxy_network_scope: Some(OutboundScope::Private),
        };

        let preserved =
            upgrade_imported_credential(build("socks5h://operator:secret@100.64.0.16:1080"))
                .unwrap();
        assert_eq!(
            preserved.proxy(),
            Some((
                "socks5h://operator:secret@100.64.0.16:1080",
                OutboundScope::Private
            ))
        );

        let error = upgrade_imported_credential(build(
            "socks5://operator:secret@proxy.service.svc.cluster.local:1080",
        ))
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid request: CPA Codex document is invalid"
        );
    }

    #[test]
    fn rejects_unknown_malformed_and_unsafe_fields_with_redacted_errors() {
        let secrets = [
            "access-token-secret",
            "refresh-token-secret",
            "id-token-secret",
            "codex@example.test",
        ];
        let mut rejected = Vec::new();

        let mut unknown = document();
        unknown["source_path"] = json!("/private/account.json");
        rejected.push(unknown);

        let mut wrong_type = document();
        wrong_type["type"] = json!("claude");
        rejected.push(wrong_type);

        let mut malformed_expiry = document();
        malformed_expiry["expired"] = json!("tomorrow");
        rejected.push(malformed_expiry);

        let mut control = document();
        control["account_id"] = json!("account\nother");
        rejected.push(control);

        let mut oversized = document();
        oversized["access_token"] = json!("x".repeat(super::super::MAX_TOKEN_BYTES + 1));
        rejected.push(oversized);

        let mut oversized_account = document();
        oversized_account["account_id"] = json!("a".repeat(super::super::MAX_ACCOUNT_ID_BYTES + 1));
        rejected.push(oversized_account);

        let mut oversized_name = document();
        oversized_name["email"] = json!("n".repeat(super::super::MAX_ACCOUNT_NAME_BYTES + 1));
        rejected.push(oversized_name);

        for payload in rejected {
            let error = normalize(&payload).unwrap_err();
            let rendered = format!("{error:?} {error}");
            assert_eq!(
                error.to_string(),
                "invalid request: CPA Codex OAuth document is invalid"
            );
            for secret in secrets {
                assert!(!rendered.contains(secret));
            }
            assert!(!rendered.contains("/private/account.json"));
        }
    }

    #[tokio::test]
    async fn refresh_uses_fixed_form_and_rotates_tokens_without_changing_state() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
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
            assert_eq!(&request[..3], &[5, 1, 0]);
            let ip = match request[3] {
                1 => {
                    let mut bytes = [0_u8; 4];
                    client.read_exact(&mut bytes).await.unwrap();
                    IpAddr::from(bytes)
                }
                4 => {
                    let mut bytes = [0_u8; 16];
                    client.read_exact(&mut bytes).await.unwrap();
                    IpAddr::from(bytes)
                }
                value => panic!("unexpected SOCKS5 address type {value}"),
            };
            let mut port = [0_u8; 2];
            client.read_exact(&mut port).await.unwrap();
            let requested = SocketAddr::new(ip, u16::from_be_bytes(port));
            assert_eq!(requested, target_address);
            let mut upstream = TcpStream::connect(requested).await.unwrap();
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
            *proxy_url = Some(format!("socks5://{proxy_address}"));
            *proxy_network_scope = Some(OutboundScope::Private);
        }
        let before = crate::db::unix_millis();
        let refreshed = refresh_at(
            &crate::build_http_client().unwrap(),
            &old_credential,
            true,
            &format!("{}/token", server.uri()),
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
        assert_eq!(rendered["proxy_url"], format!("socks5://{proxy_address}"));
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
