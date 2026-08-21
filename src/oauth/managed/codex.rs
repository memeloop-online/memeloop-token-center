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
const LEGACY_ADAPTER_SCHEMA: &str = "cpa-codex-oauth-v1";
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
                "schema": "cpa-codex-oauth-v1",
                "account_id": document.account_id,
            })),
        },
    })
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

    let client =
        network::client_for_url(http, endpoint, OutboundScope::Public, allow_test_loopback)
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
            Some(NATIVE_ADAPTER_SCHEMA | LEGACY_ADAPTER_SCHEMA)
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
    use serde_json::json;
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
        credential_with_schema(refresh_token, LEGACY_ADAPTER_SCHEMA)
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
            json!({"schema": "cpa-codex-oauth-v1", "account_id": "account-123"})
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
            json!({"schema": "cpa-codex-oauth-v1", "account_id": "account 123"}),
            json!({"schema": "cpa-codex-oauth-v1", "account_id": "账户"}),
            json!({"schema": "cpa-codex-oauth-v1", "account_id": "account-123", "extra": true}),
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
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "new-access-secret",
                "refresh_token": "new-refresh-secret",
                "expires_in": 3600,
                "token_type": "Bearer"
            })))
            .mount(&server)
            .await;
        let before = crate::db::unix_millis();
        let refreshed = refresh_at(
            &crate::build_http_client().unwrap(),
            &credential_with_schema("old-refresh-secret", NATIVE_ADAPTER_SCHEMA),
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
