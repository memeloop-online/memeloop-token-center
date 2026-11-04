use serde::{Deserialize, Serialize};

const MAX_INLINE_JSON_BYTES: usize = 128 * 1024;
mod reason;
pub(crate) use reason::UpstreamReason;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupplierError {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub category: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
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

impl UpstreamReason {
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
            retryable: Some(self.retryable()),
            action: self.action().to_owned(),
            provider_code: None,
            provider_message: None,
            provider_details: None,
        }
    }
}

/// The stream terminal records its classified reason without changing its
/// archive locator. HTTP rejections retain their native inline projection.
pub(crate) fn supplier_error_from_record(
    inline: Option<&str>,
    status_code: Option<i64>,
    error_code: Option<&str>,
) -> Option<SupplierError> {
    supplier_error_from_inline_json(inline).or_else(|| {
        status_code
            .filter(|status| *status >= 400)
            .and_then(|_| error_code.and_then(UpstreamReason::from_code))
            .map(UpstreamReason::supplier_error)
    })
}

pub fn supplier_error_from_inline_json(value: Option<&str>) -> Option<SupplierError> {
    let value = value?;
    if value.len() > MAX_INLINE_JSON_BYTES {
        return None;
    }
    let raw = value.strip_prefix("inline-json:")?;
    let envelope: InlineEnvelope<'_> = serde_json::from_str(raw).ok()?;
    let error = envelope.error;
    if error.error_type != "upstream_error" {
        return None;
    }
    let reason = error.mtc_safe_reason.and_then(UpstreamReason::from_marker);
    let mut supplier = reason
        .map(UpstreamReason::supplier_error)
        .unwrap_or_else(|| SupplierError {
            code: error.code.to_owned(),
            message: error.message.to_owned(),
            category: "upstream".to_owned(),
            retryable: None,
            action: "inspect_upstream_error".to_owned(),
            provider_code: None,
            provider_message: None,
            provider_details: None,
        });
    supplier.provider_code = error.mtc_provider_code.map(str::to_owned);
    supplier.provider_message = error
        .mtc_provider_message
        .or_else(|| (error.message != supplier.message).then_some(error.message))
        .map(str::to_owned);
    supplier.provider_details = error.mtc_provider_details;
    Some(supplier)
}

/// Classification supplements provider details; an unfamiliar code is still an
/// observable upstream error, even when we have no recovery advice for it.
pub(crate) fn reason_from_value(value: &serde_json::Value) -> Option<UpstreamReason> {
    let error = value
        .pointer("/response/error")
        .filter(|error| !error.is_null())
        .or_else(|| value.get("error"))
        .unwrap_or(value);
    let object = error.as_object()?;
    if object.contains_key("message") && object.contains_key("msg") {
        return None;
    }
    UpstreamReason::from_supplier(
        object.get("code").and_then(serde_json::Value::as_str),
        object
            .get("message")
            .or_else(|| object.get("msg"))
            .and_then(serde_json::Value::as_str),
    )
}

/// Preserve the provider's explanation while attaching optional classification.
/// Protocol adapters use this projection instead of maintaining allowlists.
pub(crate) fn upstream_error_value(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::{Value, json};
    let error = value
        .pointer("/response/error")
        .filter(|error| !error.is_null())
        .or_else(|| value.get("error"))
        .unwrap_or(value);
    if error.get("type").and_then(Value::as_str) == Some("upstream_error")
        && error.get("mtc_provider_message").is_some()
    {
        return error.clone();
    }
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
        "code": reason.map(UpstreamReason::client_code).or(code.as_deref()).unwrap_or("upstream_rejected"),
        "message": message,
        "mtc_provider_message": message,
    });
    if let Some(code) = code {
        projected["mtc_provider_code"] = json!(code);
    }
    if let Some(reason) = reason {
        projected["mtc_safe_reason"] = json!(reason.code());
    }
    if let Some(kind) = error.get("type") {
        projected["mtc_provider_type"] = kind.clone();
    }
    if let Some(param) = error.get("param") {
        projected["param"] = param.clone();
    }
    if let Some(fields) = error.as_object() {
        let details: serde_json::Map<String, Value> = fields
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "code" | "message" | "msg"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if !details.is_empty() {
            projected["mtc_provider_details"] = Value::Object(details);
        }
    }
    projected
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
                assert_eq!(record.retryable, (category != "upstream").then_some(false));
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
