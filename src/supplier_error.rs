use serde::{Deserialize, Serialize};

mod reason;
pub(crate) use reason::SafeReason;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupplierError {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default)]
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_details: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct InlineEnvelope<'a> {
    #[serde(borrow)]
    error: InlineError<'a>,
}

#[derive(Deserialize)]
struct InlineError<'a> {
    #[serde(rename = "type")]
    error_type: &'a str,
    code: &'a str,
    message: &'a str,
    #[serde(default)]
    mtc_safe_reason: Option<&'a str>,
    #[serde(default)]
    mtc_provider_code: Option<&'a str>,
    #[serde(default)]
    mtc_provider_message: Option<&'a str>,
    #[serde(default)]
    mtc_provider_details: Option<serde_json::Value>,
}

impl SafeReason {
    pub(crate) fn from_supplier(code: Option<&str>, message: Option<&str>) -> Option<Self> {
        let code_reason = code.and_then(Self::from_code);
        let message_reason = message.and_then(|message| match message {
            "当前账号没有可用套餐" | "No active plan" | "No available plan" => {
                Some(Self::NoActivePlan)
            }
            "Quota exhausted"
            | "Quota exhausted for this account"
            | "Insufficient quota"
            | "用户额度不足" => Some(Self::InsufficientQuota),
            "Invalid API key" | "Invalid authentication credentials" => {
                Some(Self::AuthenticationInvalid)
            }
            "API key expired" | "Authentication token expired" => Some(Self::AuthenticationExpired),
            "Rate limit exceeded" | "Too many requests" => Some(Self::RateLimited),
            "Model unavailable" | "Unsupported model" | "Model not found" => {
                Some(Self::ModelUnavailable)
            }
            _ => None,
        });
        match (code_reason, message_reason) {
            (Some(code), Some(message)) if code != message => None,
            (Some(reason), _) | (_, Some(reason)) => Some(reason),
            _ => None,
        }
    }

    fn supplier_error(self) -> SupplierError {
        SupplierError {
            code: self.code().to_owned(),
            message: self.message().to_owned(),
            category: self.category().to_owned(),
            retryable: self.retryable(),
            action: self.action().to_owned(),
            provider_code: None,
            provider_message: None,
            provider_details: None,
        }
    }
}

/// The stream terminal records this existing safe reason without changing its
/// archive locator. New HTTP rejections use a separate fixed-vocabulary object;
/// this parser also validates the legacy projections during migration.
pub(crate) fn supplier_error_from_record(
    inline: Option<&str>,
    status_code: Option<i64>,
    error_code: Option<&str>,
) -> Option<SupplierError> {
    supplier_error_from_inline_json(inline).or_else(|| {
        status_code
            .filter(|status| *status >= 400)
            .and_then(|_| error_code.and_then(SafeReason::from_code))
            .map(SafeReason::supplier_error)
    })
}

pub fn supplier_error_from_inline_json(value: Option<&str>) -> Option<SupplierError> {
    let value = value?;
    let raw = value.strip_prefix("inline-json:")?;
    let envelope: InlineEnvelope<'_> = serde_json::from_str(raw).ok()?;
    let error = envelope.error;
    if error.error_type != "upstream_error" {
        return None;
    }
    let reason = error.mtc_safe_reason.and_then(SafeReason::from_marker);
    let mut supplier = reason
        .map(SafeReason::supplier_error)
        .unwrap_or_else(|| SupplierError {
            code: error.code.to_owned(),
            message: error.message.to_owned(),
            category: "upstream".to_owned(),
            retryable: false,
            action: "inspect_upstream_error".to_owned(),
            provider_code: None,
            provider_message: None,
            provider_details: None,
        });
    supplier.provider_code = error.mtc_provider_code.map(str::to_owned);
    supplier.provider_message = error.mtc_provider_message.map(str::to_owned);
    supplier.provider_details = error.mtc_provider_details;
    Some(supplier)
}

/// Classification supplements provider details; an unfamiliar code is still an
/// observable upstream error, even when we have no recovery advice for it.
pub(crate) fn reason_from_value(value: &serde_json::Value) -> Option<SafeReason> {
    let error = value
        .pointer("/response/error")
        .filter(|error| !error.is_null())
        .or_else(|| value.get("error"))
        .unwrap_or(value);
    let object = error.as_object()?;
    if object.contains_key("message") && object.contains_key("msg") {
        return None;
    }
    SafeReason::from_supplier(
        object.get("code").and_then(serde_json::Value::as_str),
        object
            .get("message")
            .or_else(|| object.get("msg"))
            .and_then(serde_json::Value::as_str),
    )
}

fn credential_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
    matches!(
        normalized.as_str(),
        "token"
            | "accesstoken"
            | "refreshtoken"
            | "idtoken"
            | "apikey"
            | "authorization"
            | "proxyauthorization"
            | "password"
            | "secret"
            | "clientsecret"
            | "credential"
            | "credentials"
            | "cookie"
            | "setcookie"
            | "signature"
            | "sig"
    ) || normalized.starts_with("xamz")
        || normalized.starts_with("xgoog")
}

fn collect_credentials(value: &serde_json::Value, secrets: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                if credential_key(key) {
                    if let Some(secret) = value.as_str().filter(|secret| !secret.is_empty()) {
                        secrets.push(secret.to_owned());
                        if matches!(
                            key.to_ascii_lowercase().replace(['-', '_'], "").as_str(),
                            "authorization" | "proxyauthorization"
                        ) {
                            if let Some((scheme, credential)) = secret.split_once(' ') {
                                if (scheme.eq_ignore_ascii_case("bearer")
                                    || scheme.eq_ignore_ascii_case("basic"))
                                    && !credential.trim().is_empty()
                                {
                                    secrets.push(credential.trim().to_owned());
                                }
                            }
                        }
                    }
                }
                collect_credentials(value, secrets);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_credentials(value, secrets);
            }
        }
        serde_json::Value::String(text) => {
            if let Ok(parsed) = url::Url::parse(text) {
                if matches!(parsed.scheme(), "http" | "https") {
                    if let Some(password) = parsed.password().filter(|v| !v.is_empty()) {
                        secrets.push(password.to_owned());
                        secrets.push(
                            percent_encoding::percent_decode_str(password)
                                .decode_utf8_lossy()
                                .into_owned(),
                        );
                    }
                    for (key, value) in parsed.query_pairs() {
                        if credential_key(&key) && !value.is_empty() {
                            secrets.push(value.into_owned());
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Remove explicit credentials, retaining provider explanations and extensions.
fn redact_credential_fields(value: &mut serde_json::Value, secrets: &[String]) {
    match value {
        serde_json::Value::Object(fields) => {
            fields.retain(|key, _| !credential_key(key));
            for value in fields.values_mut() {
                redact_credential_fields(value, secrets);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_credential_fields(value, secrets);
            }
        }
        serde_json::Value::String(text) => {
            if let Ok(mut parsed) = url::Url::parse(text) {
                if matches!(parsed.scheme(), "http" | "https") {
                    let pairs: Vec<(String, String)> = parsed
                        .query_pairs()
                        .filter(|(key, _)| !credential_key(key))
                        .map(|(key, value)| (key.into_owned(), value.into_owned()))
                        .collect();
                    if pairs.len() != parsed.query_pairs().count()
                        || !parsed.username().is_empty()
                        || parsed.password().is_some()
                    {
                        let _ = parsed.set_username("");
                        let _ = parsed.set_password(None);
                        parsed.set_query(None);
                        if !pairs.is_empty() {
                            parsed.query_pairs_mut().extend_pairs(pairs);
                        }
                        *text = parsed.into();
                    }
                }
            }
            for secret in secrets {
                // Replace exact credential tokens; boundaries avoid corrupting ordinary words.
                let mut result = String::new();
                let mut cursor = 0;
                for (offset, _) in text.match_indices(secret) {
                    let end = offset + secret.len();
                    let boundary = |c: char| !c.is_alphanumeric() && c != '_';
                    if text[..offset].chars().next_back().is_none_or(boundary)
                        && text[end..].chars().next().is_none_or(boundary)
                    {
                        result.push_str(&text[cursor..offset]);
                        result.push_str("[redacted]");
                        cursor = end;
                    }
                }
                result.push_str(&text[cursor..]);
                *text = result;
            }
        }
        _ => {}
    }
}

/// Preserve the provider's explanation while attaching optional classification.
/// Protocol adapters use this projection instead of maintaining allowlists.
pub(crate) fn upstream_error_value(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::{Value, json};
    let mut redacted = value.clone();
    let mut secrets = Vec::new();
    collect_credentials(value, &mut secrets);
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets.dedup();
    redact_credential_fields(&mut redacted, &secrets);
    let value = &redacted;
    let error = value
        .pointer("/response/error")
        .filter(|error| !error.is_null())
        .or_else(|| value.get("error"))
        .unwrap_or(value);
    let code = error.get("code").and_then(|code| match code {
        Value::String(code) => Some(code.clone()),
        Value::Number(code) => Some(code.to_string()),
        _ => None,
    });
    let message = error
        .get("message")
        .or_else(|| error.get("msg"))
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| error.to_string());
    let reason = reason_from_value(value);
    let mut projected = json!({
        "type": "upstream_error",
        "code": reason.map(SafeReason::client_code).or(code.as_deref()).unwrap_or("upstream_rejected"),
        "message": message,
        "mtc_provider_message": message,
        "mtc_provider_details": value,
    });
    if let Some(code) = code {
        projected["mtc_provider_code"] = json!(code);
    }
    if let Some(reason) = reason {
        projected["mtc_safe_reason"] = json!(reason.code());
    }
    projected
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn credential_echoes_include_authorization_payload_and_decoded_password() {
        let raw = json!({"error": {"authorization": "Bearer actual-secret", "url": "https://user:private%2Dpassword@provider.invalid/result", "message": "explanation actual-secret private-password"}});
        let projected = upstream_error_value(&raw);
        assert_eq!(projected["message"], "explanation [redacted] [redacted]");
        assert_eq!(
            projected["mtc_provider_details"]["error"]["url"],
            "https://provider.invalid/result"
        );
        for secret in ["actual-secret", "private-password", "private%2Dpassword"] {
            assert!(!projected.to_string().contains(secret));
        }
    }

    #[test]
    fn credential_projection_handles_url_and_known_secret_echoes() {
        let raw = json!({"error": {
            "message": "provider explanation token=actual-credential; ordinary words stay",
            "token": "actual-credential",
            "extension": "echo actual-credential",
            "url": "https://user:private-password@provider.invalid/result?Refresh%5FToken=encoded-secret&Authorization=query-secret&format=png"
        }});
        let projected = upstream_error_value(&raw);
        assert_eq!(
            projected["message"],
            "provider explanation token=[redacted]; ordinary words stay"
        );
        let rendered = projected.to_string();
        for secret in [
            "actual-credential",
            "private-password",
            "encoded-secret",
            "query-secret",
            "user:",
        ] {
            assert!(!rendered.contains(secret));
        }
        assert_eq!(
            projected["mtc_provider_details"]["error"]["url"],
            "https://provider.invalid/result?format=png"
        );
    }

    #[test]
    fn shared_projection_preserves_explanation_and_removes_credentials() {
        let value = json!({"error": {"message": "provider explanation", "token": "credential-fixture", "extension": "x".repeat(140_000), "nested": [{"authorization": "Bearer private", "signed_url": "https://provider.invalid/result?token=signed-secret&format=png"}]}});
        let projected = upstream_error_value(&value);
        assert_eq!(projected["message"], "provider explanation");
        assert_eq!(
            projected["mtc_provider_details"]["error"]["extension"],
            "x".repeat(140_000)
        );
        let rendered = projected.to_string();
        for secret in ["credential-fixture", "Bearer private", "signed-secret"] {
            assert!(!rendered.contains(secret));
        }
        assert!(rendered.contains("format=png"));
    }

    #[test]
    fn record_details_above_old_inline_limit_preserve_unknown_fields() {
        let raw = json!({"error":{"code":"provider_future_code","message":"complete", "param":"input", "extension":"x".repeat(140_000)}});
        let projected = upstream_error_value(&raw);
        let stored = format!("inline-json:{}", json!({"error":projected}));
        let supplier = supplier_error_from_inline_json(Some(&stored)).unwrap();
        assert_eq!(supplier.provider_details, Some(raw));
        assert_eq!(
            supplier.provider_code.as_deref(),
            Some("provider_future_code")
        );
    }

    #[test]
    fn known_and_unknown_details_survive_http_stream_and_record_projection() {
        for (code, message, category) in [
            (
                "misalignment_policy_violation",
                "This request was blocked by our safety systems. Reason: Potentially unintended activity.",
                "policy",
            ),
            (
                "insufficient_user_quota",
                "用户额度不足, 剩余额度: -0.000160 (request id: quota-fixture)",
                "quota",
            ),
            (
                "new_provider_code",
                "Unsupported tool schema: tools[3].parameters",
                "upstream",
            ),
        ] {
            for value in [
                json!({"error": {"code":code,"message":message}}),
                json!({"response": {"error": {"code":code,"message":message}}}),
                json!({"code":code,"message":message}),
            ] {
                let error = upstream_error_value(&value);
                assert_eq!(error["message"], message);
                assert_eq!(error["mtc_provider_code"], code);
                let inline = format!("inline-json:{}", json!({"error": error}));
                let record =
                    supplier_error_from_record(Some(&inline), Some(502), Some("http_502")).unwrap();
                assert_eq!(record.provider_message.as_deref(), Some(message));
                assert_eq!(record.provider_code.as_deref(), Some(code));
                assert_eq!(record.category, category);
                assert!(!record.retryable);
            }
        }
    }

    #[test]
    fn legacy_native_envelopes_and_additive_fields_remain_readable() {
        let inline = r#"inline-json:{"error":{"type":"upstream_error","code":"no_active_plan","message":"当前账号没有可用套餐","mtc_safe_reason":"no_active_plan","future_field":true}}"#;
        let error = supplier_error_from_inline_json(Some(inline)).unwrap();
        assert_eq!(error.action, "check_plan");
        assert!(error.provider_message.is_none());
        assert!(supplier_error_from_record(None, Some(200), Some("no_active_plan")).is_none());
    }

    #[test]
    fn classification_does_not_erase_conflicting_provider_details() {
        let value = json!({"error":{"code":"context_length_exceeded","message":"Quota exhausted"}});
        assert_eq!(reason_from_value(&value), None);
        let error = upstream_error_value(&value);
        assert_eq!(error["code"], "context_length_exceeded");
        assert_eq!(error["message"], "Quota exhausted");
    }
}
