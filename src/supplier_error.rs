use serde::{
    Deserialize, Serialize,
    de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor},
};

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
    #[serde(default)]
    mtc_provider_code: Option<&'a str>,
    #[serde(default)]
    mtc_provider_message: Option<&'a str>,
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

    pub(crate) fn provider_detail<'a>(
        self,
        code: Option<&'a str>,
        message: Option<&'a str>,
    ) -> (Option<&'a str>, Option<&'a str>) {
        (
            code.filter(|code| {
                Self::from_supplier(Some(code), None) == Some(self)
                    || (*code == "402" && self == Self::NoActivePlan)
                    || (*code == "400" && self == Self::ModelUnavailable)
            }),
            message.filter(|message| Self::from_supplier(None, Some(message)) == Some(self)),
        )
    }
}

pub(crate) fn invalid_native_supplier_envelope(location: &str) -> bool {
    let Some(raw) = location.strip_prefix("inline-json:") else {
        return false;
    };
    let mut marked = false;
    let prefix = &raw.as_bytes()[..raw.len().min(MAX_INLINE_JSON_BYTES)];
    let mut parser = serde_json::Deserializer::from_slice(prefix);
    let _ = serde::Deserializer::deserialize_map(&mut parser, NativeEnvelopeMarker(&mut marked));
    marked && supplier_error_from_inline_json(Some(location)).is_none()
}

struct NativeEnvelopeMarker<'a>(&'a mut bool);

impl<'de> Visitor<'de> for NativeEnvelopeMarker<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an inline envelope")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key::<String>()? {
            if key == "error" {
                map.next_value_seed(NativeErrorMarker(&mut *self.0))?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(())
    }
}

struct NativeErrorMarker<'a>(&'a mut bool);

impl<'de> DeserializeSeed<'de> for NativeErrorMarker<'_> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for NativeErrorMarker<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an error value")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key::<String>()? {
            if matches!(
                key.as_str(),
                "mtc_safe_reason" | "mtc_provider_code" | "mtc_provider_message"
            ) {
                *self.0 = true;
            }
            map.next_value::<IgnoredAny>()?;
        }
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(())
    }

    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<(), E> {
        Ok(())
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
    let (code, message) =
        reason.provider_detail(error.mtc_provider_code, error.mtc_provider_message);
    if code != error.mtc_provider_code || message != error.mtc_provider_message {
        return None;
    }
    let mut supplier = reason.supplier_error();
    if let Some(code) = code {
        supplier
            .message
            .push_str(&format!("; provider code: {code}"));
    }
    if let Some(message) = message {
        supplier
            .message
            .push_str(&format!("; provider message: {message}"));
    }
    Some(supplier)
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
    fn native_markers_survive_incomplete_and_duplicate_parsing() {
        for raw in [
            r#"{"error":{"mtc_safe_reason":"model_unavailable","mtc_provider_message":"Authorization: Bearer private-canary"}} trailing"#,
            r#"{"error":{"mtc_safe_reason":"model_unavailable","mtc_provider_message":"Authorization: Bearer private-canary"},"error":{"message":"legacy"}}"#,
            r#"{"\u0065rror":{"\u006dtc_provider_message":"Authorization: Bearer private-canary""#,
            r#"{"error":{"mtc_safe_reason":"model_unavailable","mtc_safe_reason":"model_unavailable"}}"#,
        ] {
            let location = format!("inline-json:{raw}");
            assert!(supplier_error_from_inline_json(Some(&location)).is_none());
            assert!(invalid_native_supplier_envelope(&location));
        }
        for raw in [
            r#"{"error":{"message":"mtc_safe_reason"}} trailing"#,
            r#"{"error":{"debug":{"mtc_provider_message":"legacy data"}}}"#,
            "data: {\"error\":{\"mtc_safe_reason\":\"legacy SSE text\"}}\n\n",
        ] {
            assert!(!invalid_native_supplier_envelope(&format!(
                "inline-json:{raw}"
            )));
        }
    }

    #[test]
    fn provider_detail_is_revalidated_and_legacy_envelopes_remain_readable() {
        for (reason, code, message) in [
            (
                SafeReason::NoActivePlan,
                "NoAvailablePlan",
                "No active plan",
            ),
            (SafeReason::NoActivePlan, "402", "当前账号没有可用套餐"),
            (
                SafeReason::ModelUnavailable,
                "model_not_found",
                "Model not found",
            ),
            (
                SafeReason::ModelUnavailable,
                "unsupported_model",
                "Unsupported model",
            ),
            (SafeReason::ModelUnavailable, "400", "Model unavailable"),
        ] {
            let mut envelope: Value =
                serde_json::from_str(inline(reason).strip_prefix("inline-json:").unwrap()).unwrap();
            envelope["error"]["mtc_provider_code"] = json!(code);
            envelope["error"]["mtc_provider_message"] = json!(message);
            let projected =
                supplier_error_from_inline_json(Some(&format!("inline-json:{envelope}"))).unwrap();
            assert_eq!(projected.code, reason.code());
            assert_eq!(
                projected.message,
                format!(
                    "{}; provider code: {code}; provider message: {message}",
                    reason.message()
                )
            );
            for field in ["mtc_provider_code", "mtc_provider_message"] {
                for unsafe_text in [
                    "Authorization: Bearer private-canary",
                    "api_key=private-canary",
                    "https://private.invalid/?X-Amz-Signature=private-canary",
                    "No active plan private prompt",
                    "private%20prompt",
                    "",
                    &"x".repeat(4096),
                    "rate_limit_exceeded",
                ] {
                    let mut tampered = envelope.clone();
                    tampered["error"][field] = json!(unsafe_text);
                    assert_eq!(
                        supplier_error_from_inline_json(Some(&format!("inline-json:{tampered}"))),
                        None
                    );
                }
            }
            assert_eq!(
                supplier_error_from_inline_json(Some(&inline(reason))),
                Some(reason.supplier_error())
            );
        }
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
