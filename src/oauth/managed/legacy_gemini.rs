use serde::Deserialize;
use serde_json::{Value, json};

use crate::{error::AppError, oauth::ManagedOAuthNormalizedAccount, provider::UpstreamCredential};

pub const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
const BASE_URL: &str = "https://cloudcode-pa.googleapis.com";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyGeminiDocument {
    #[serde(rename = "type")]
    provider_type: String,
    token: LegacyGeminiToken,
    project_id: String,
    #[serde(default)]
    email: Option<String>,
    auto: bool,
    checked: bool,
    #[serde(default)]
    disabled: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyGeminiToken {
    access_token: String,
    refresh_token: String,
    token_type: String,
    expiry: String,
    #[serde(default)]
    token_uri: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_secret: Option<String>,
    #[serde(default)]
    scopes: Option<Vec<String>>,
    #[serde(default)]
    universe_domain: Option<String>,
}

pub fn normalize(payload: &Value) -> Result<ManagedOAuthNormalizedAccount, AppError> {
    let document: LegacyGeminiDocument =
        serde_json::from_value(payload.clone()).map_err(|_| invalid_document())?;
    if document.provider_type != "gemini" || document.token.token_type != "Bearer" {
        return Err(invalid_document());
    }
    super::bearer_token(&document.token.access_token, "legacy CPA Gemini")?;
    super::required_secret(&document.token.refresh_token, "legacy CPA Gemini")?;
    super::optional_secret(document.token.client_id.as_deref(), "legacy CPA Gemini")?;
    super::optional_secret(document.token.client_secret.as_deref(), "legacy CPA Gemini")?;
    validate_ignored_metadata(&document.token)?;
    super::project_id(&document.project_id, "legacy CPA Gemini")?;
    let account_name = super::account_name(
        document.email.as_deref(),
        "Gemini account",
        "legacy CPA Gemini",
    )?;
    let expires_at = super::timestamp_millis(&document.token.expiry, "legacy CPA Gemini")?;
    let _ = (document.auto, document.checked);

    Ok(ManagedOAuthNormalizedAccount {
        account_name,
        config: json!({
            "base_url": BASE_URL,
            "network_scope": "public",
        }),
        enabled: !document.disabled,
        credential: UpstreamCredential::OAuth {
            access_token: document.token.access_token,
            refresh_token: Some(document.token.refresh_token),
            expires_at: Some(expires_at),
            header: "authorization".to_owned(),
            prefix: "Bearer ".to_owned(),
            adapter_state: Some(json!({
                "schema": "cpa-gemini-cli-oauth-v1",
                "project_id": document.project_id,
            })),
        },
    })
}

fn validate_ignored_metadata(token: &LegacyGeminiToken) -> Result<(), AppError> {
    for value in [token.token_uri.as_deref(), token.universe_domain.as_deref()]
        .into_iter()
        .flatten()
    {
        super::controlled_text(value, 2_048, false, "legacy CPA Gemini")?;
    }
    if let Some(scopes) = &token.scopes {
        if scopes.len() > 32 {
            return Err(invalid_document());
        }
        for scope in scopes {
            super::controlled_text(scope, 2_048, false, "legacy CPA Gemini")?;
        }
    }
    Ok(())
}

pub fn refresh_unavailable() -> Result<UpstreamCredential, AppError> {
    Err(AppError::Upstream(
        "legacy CPA Gemini OAuth refresh is unavailable".into(),
    ))
}

fn invalid_document() -> AppError {
    super::invalid_document("legacy CPA Gemini")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn document() -> Value {
        json!({
            "type": "gemini",
            "token": {
                "access_token": "gemini-access-secret",
                "refresh_token": "gemini-refresh-secret",
                "token_type": "Bearer",
                "expiry": "2099-01-01T00:00:00Z",
                "token_uri": "https://oauth2.googleapis.com/token",
                "client_id": "fixture-google-client-id",
                "client_secret": "fixture-google-client-secret",
                "scopes": [
                    "https://www.googleapis.com/auth/cloud-platform",
                    "https://www.googleapis.com/auth/userinfo.email",
                    "https://www.googleapis.com/auth/userinfo.profile"
                ],
                "universe_domain": "googleapis.com"
            },
            "project_id": "safe-project-123",
            "email": "gemini@example.test",
            "auto": true,
            "checked": true
        })
    }

    #[test]
    fn normalizes_valid_disabled_and_expired_legacy_documents() {
        let normalized = normalize(&document()).unwrap();
        assert!(normalized.enabled);
        assert_eq!(normalized.account_name, "gemini@example.test");
        assert_eq!(normalized.config["base_url"], BASE_URL);
        let serialized = serde_json::to_value(&normalized.credential).unwrap();
        assert_eq!(serialized["expires_at"], 4_070_908_800_000_i64);
        assert_eq!(
            serialized["adapter_state"],
            json!({
                "schema": "cpa-gemini-cli-oauth-v1",
                "project_id": "safe-project-123"
            })
        );
        let state = serialized["adapter_state"].to_string();
        assert!(!state.contains("gemini@example.test"));
        assert!(
            !serde_json::to_string(&normalized)
                .unwrap()
                .contains("fixture-google-client")
        );
        let debug = format!("{normalized:?}");
        assert!(!debug.contains("gemini-access-secret"));
        assert!(!debug.contains("gemini-refresh-secret"));
        assert!(!debug.contains("fixture-google-client-secret"));

        let mut disabled = document();
        disabled["disabled"] = json!(true);
        assert!(!normalize(&disabled).unwrap().enabled);

        let mut expired = document();
        expired["token"]["expiry"] = json!("2020-01-01T00:00:00Z");
        assert!(
            normalize(&expired)
                .unwrap()
                .credential
                .expires_at()
                .is_some_and(|value| value < crate::db::unix_millis())
        );

        let mut unnamed = document();
        unnamed.as_object_mut().unwrap().remove("email");
        assert_eq!(normalize(&unnamed).unwrap().account_name, "Gemini account");
    }

    #[test]
    fn rejects_unknown_and_malformed_legacy_documents_without_secret_echo() {
        let mut cases = Vec::new();
        let mut unknown = document();
        unknown["quota_project"] = json!("unknown-project-secret");
        cases.push(unknown);
        let mut token_unknown = document();
        token_unknown["token"]["scope"] = json!("hidden-scope");
        cases.push(token_unknown);
        let mut wrong_type = document();
        wrong_type["type"] = json!("codex");
        cases.push(wrong_type);
        let mut wrong_token_type = document();
        wrong_token_type["token"]["token_type"] = json!("Basic");
        cases.push(wrong_token_type);
        let mut malformed_expiry = document();
        malformed_expiry["token"]["expiry"] = json!("tomorrow");
        cases.push(malformed_expiry);
        let mut unsafe_project = document();
        unsafe_project["project_id"] = json!("project\nother");
        cases.push(unsafe_project);
        let mut unsafe_client_secret = document();
        unsafe_client_secret["token"]["client_secret"] =
            json!("google-client-secret-must-not-exist\nother");
        cases.push(unsafe_client_secret);
        let mut excessive_scopes = document();
        excessive_scopes["token"]["scopes"] = json!(vec!["scope"; 33]);
        cases.push(excessive_scopes);
        let mut oversized_project = document();
        oversized_project["project_id"] = json!("p".repeat(super::super::MAX_PROJECT_ID_BYTES + 1));
        cases.push(oversized_project);
        let mut oversized_name = document();
        oversized_name["email"] = json!("n".repeat(super::super::MAX_ACCOUNT_NAME_BYTES + 1));
        cases.push(oversized_name);

        for payload in cases {
            let error = normalize(&payload).unwrap_err();
            let rendered = format!("{error:?} {error}");
            assert_eq!(
                error.to_string(),
                "invalid request: legacy CPA Gemini OAuth document is invalid"
            );
            for secret in [
                "gemini-access-secret",
                "gemini-refresh-secret",
                "google-client-secret-must-not-exist",
                "fixture-google-client-secret",
                "unknown-project-secret",
            ] {
                assert!(!rendered.contains(secret));
            }
        }
    }

    #[test]
    fn refresh_is_explicitly_unavailable_and_contains_no_google_secret() {
        let error = refresh_unavailable().unwrap_err();
        let rendered = format!("{error:?} {error}");
        assert_eq!(
            error.to_string(),
            "configured upstream is unavailable: legacy CPA Gemini OAuth refresh is unavailable"
        );
        assert!(!rendered.to_ascii_lowercase().contains("client_secret"));
    }
}
