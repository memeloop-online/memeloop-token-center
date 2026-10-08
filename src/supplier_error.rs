use serde::{Deserialize, Serialize};

const MAX_INLINE_JSON_BYTES: usize = 2048;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SupplierError {
    pub code: String,
    pub message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InlineEnvelope<'a> {
    #[serde(borrow)]
    error: InlineError<'a>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InlineError<'a> {
    #[serde(rename = "type")]
    error_type: &'a str,
    code: &'a str,
    message: &'a str,
    mtc_safe_reason: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SafeReason {
    NoActivePlan,
    InsufficientQuota,
    AuthenticationInvalid,
    AuthenticationExpired,
    RateLimited,
    ModelUnavailable,
}

impl SafeReason {
    pub(crate) fn from_marker(marker: &str) -> Option<Self> {
        match marker {
            "no_active_plan" => Some(Self::NoActivePlan),
            "insufficient_quota" => Some(Self::InsufficientQuota),
            "authentication_invalid" => Some(Self::AuthenticationInvalid),
            "authentication_expired" => Some(Self::AuthenticationExpired),
            "rate_limited" => Some(Self::RateLimited),
            "model_unavailable" => Some(Self::ModelUnavailable),
            _ => None,
        }
    }

    pub(crate) fn from_supplier(code: Option<&str>, message: Option<&str>) -> Option<Self> {
        let code_reason = code.and_then(|code| match code {
            "no_active_plan" | "NoAvailablePlan" | "NoActivePlan" | "NoPlan" => {
                Some(Self::NoActivePlan)
            }
            "insufficient_quota" => Some(Self::InsufficientQuota),
            "authentication_invalid" | "invalid_api_key" => Some(Self::AuthenticationInvalid),
            "authentication_expired" | "api_key_expired" | "token_expired" => {
                Some(Self::AuthenticationExpired)
            }
            "rate_limited" | "rate_limit_exceeded" => Some(Self::RateLimited),
            "model_unavailable" | "model_not_found" | "unsupported_model" => {
                Some(Self::ModelUnavailable)
            }
            _ => None,
        });
        let message_reason = message.and_then(|message| match message {
            "当前账号没有可用套餐" | "No active plan" | "No available plan" => {
                Some(Self::NoActivePlan)
            }
            "Quota exhausted" | "Quota exhausted for this account" | "Insufficient quota" => {
                Some(Self::InsufficientQuota)
            }
            "Invalid API key" | "Invalid authentication credentials" => {
                Some(Self::AuthenticationInvalid)
            }
            "API key expired" | "Authentication token expired" => Some(Self::AuthenticationExpired),
            "Rate limit exceeded" | "Too many requests" => Some(Self::RateLimited),
            "Model unavailable" | "Unsupported model" => Some(Self::ModelUnavailable),
            _ => None,
        });
        match (code_reason, message_reason) {
            (Some(code), Some(message)) if code != message => None,
            (Some(reason), _) | (_, Some(reason)) => Some(reason),
            _ => None,
        }
    }

    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::NoActivePlan => "no_active_plan",
            Self::InsufficientQuota => "insufficient_quota",
            Self::AuthenticationInvalid => "authentication_invalid",
            Self::AuthenticationExpired => "authentication_expired",
            Self::RateLimited => "rate_limited",
            Self::ModelUnavailable => "model_unavailable",
        }
    }

    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::NoActivePlan => "当前账号没有可用套餐",
            Self::InsufficientQuota => "The upstream account has insufficient quota",
            Self::AuthenticationInvalid => "The upstream authentication credentials are invalid",
            Self::AuthenticationExpired => "The upstream authentication credentials have expired",
            Self::RateLimited => "The upstream request rate limit was exceeded",
            Self::ModelUnavailable => "The requested upstream model is unavailable",
        }
    }

    fn supplier_error(self) -> SupplierError {
        SupplierError {
            code: self.code().to_owned(),
            message: self.message().to_owned(),
        }
    }
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
    let reason = SafeReason::from_marker(error.mtc_safe_reason)?;
    if error.code != reason.code() || error.message != reason.message() {
        return None;
    }
    Some(reason.supplier_error())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn inline(reason: SafeReason) -> String {
        format!(
            "inline-json:{}",
            json!({"error": {
                "type": "upstream_error", "code": reason.code(),
                "message": reason.message(), "mtc_safe_reason": reason.code()
            }})
        )
    }

    #[test]
    fn constructed_reasons_round_trip_only_through_native_marker_envelopes() {
        for reason in [
            SafeReason::NoActivePlan,
            SafeReason::InsufficientQuota,
            SafeReason::AuthenticationInvalid,
            SafeReason::AuthenticationExpired,
            SafeReason::RateLimited,
            SafeReason::ModelUnavailable,
        ] {
            assert_eq!(
                supplier_error_from_inline_json(Some(&inline(reason))),
                Some(reason.supplier_error())
            );
        }
        assert_eq!(supplier_error_from_inline_json(None), None);
        for raw in [
            "inline-json:{}",
            "inline-json:{\"error\":{\"type\":\"upstream_error\",\"message\":\"upstream rejected the request\"}}",
            "inline-json:{\"error\":{\"mtc_safe_reason\":\"no_active_plan\",\"message\":\"private-canary\"}}",
            "inline-json:{\"error\":{\"mtc_safe_reason\":\"no_active_plan\",\"mtc_safe_reason\":\"rate_limited\"}}",
            "inline-json:{",
        ] {
            assert_eq!(supplier_error_from_inline_json(Some(raw)), None);
        }
        let good = inline(SafeReason::NoActivePlan);
        assert_eq!(
            supplier_error_from_inline_json(Some(good.strip_prefix("inline-json:").unwrap())),
            None
        );
        assert_eq!(
            supplier_error_from_inline_json(Some(&"x".repeat(MAX_INLINE_JSON_BYTES + 1))),
            None
        );
    }

    #[test]
    fn stored_text_and_marker_cannot_expand_the_fixed_vocabulary() {
        for field in ["code", "message", "mtc_safe_reason", "type"] {
            let good = inline(SafeReason::NoActivePlan);
            let mut value: Value =
                serde_json::from_str(good.strip_prefix("inline-json:").unwrap()).unwrap();
            value["error"][field] = json!("private partial%2Fcanary");
            assert_eq!(
                supplier_error_from_inline_json(Some(&format!("inline-json:{value}"))),
                None
            );
        }
    }
}
