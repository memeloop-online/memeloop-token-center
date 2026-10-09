use super::*;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum UsageRejection {
    InvalidType(&'static str),
    InvalidCachedType(&'static str),
    OutOfRange(&'static str),
    MissingPromptTokens,
    InvalidPromptAlias,
    ConflictingCacheAliases,
    CacheNonconservation,
    CacheWriteOverflow,
    InputArithmeticOverflow,
    UnsupportedServiceTier,
    InvalidJson,
    ProviderNormalization,
    UnclassifiedExtraction,
}

fn value_shape(value: Option<&Value>) -> Value {
    let kind = match value {
        None => "absent",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(number)) if number.is_i64() => "integer",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    };
    serde_json::json!({"type": kind})
}

fn usage_shape(value: &Value) -> Value {
    let (location, usage) = if let Some(usage) = value.get("usage") {
        ("usage", Some(usage))
    } else if let Some(usage) = value.pointer("/message/usage") {
        ("message.usage", Some(usage))
    } else {
        ("response.usage", value.pointer("/response/usage"))
    };
    let mut shape = serde_json::json!({
        "location": location,
        "usage": value_shape(usage),
        "choices": value_shape(value.get("choices")),
        "error": value_shape(value.get("error")),
        "service_tier": value_shape(value.get("service_tier").or_else(|| value.pointer("/response/service_tier")))
    });
    for (name, pointer) in [
        ("input_tokens", "/input_tokens"),
        ("prompt_tokens", "/prompt_tokens"),
        ("output_tokens", "/output_tokens"),
        ("completion_tokens", "/completion_tokens"),
        ("total_tokens", "/total_tokens"),
        ("prompt_cache_hit_tokens", "/prompt_cache_hit_tokens"),
        ("prompt_cache_miss_tokens", "/prompt_cache_miss_tokens"),
        ("cache_read_input_tokens", "/cache_read_input_tokens"),
        (
            "cache_creation_input_tokens",
            "/cache_creation_input_tokens",
        ),
        ("input_tokens_details", "/input_tokens_details"),
        ("input_cached_tokens", "/input_tokens_details/cached_tokens"),
        ("prompt_tokens_details", "/prompt_tokens_details"),
        (
            "prompt_cached_tokens",
            "/prompt_tokens_details/cached_tokens",
        ),
        ("cache_creation", "/cache_creation"),
        (
            "ephemeral_5m_input_tokens",
            "/cache_creation/ephemeral_5m_input_tokens",
        ),
        (
            "ephemeral_1h_input_tokens",
            "/cache_creation/ephemeral_1h_input_tokens",
        ),
    ] {
        let value = usage.and_then(|usage| usage.pointer(pointer));
        let mut field_shape = value_shape(value);
        if name.ends_with("_tokens")
            && let Some(count) = value.and_then(Value::as_i64)
        {
            field_shape["count"] = Value::from(count);
        }
        shape[name] = field_shape;
    }
    shape
}

pub(in crate::api::proxy) fn observe_buffered_usage_rejection(
    request_id: Uuid,
    status: u16,
    body: &[u8],
    driver: &str,
    protocol: Protocol,
) {
    let (reason, shape) = match serde_json::from_slice::<Value>(body) {
        Ok(value) => {
            let reason = if driver == crate::oauth::managed::kimi::PROVIDER_DRIVER
                && matches!(protocol, Protocol::OpenAiChat)
            {
                UsageRejection::ProviderNormalization
            } else {
                usage_from_value_diagnosed(&value)
                    .err()
                    .unwrap_or(UsageRejection::UnclassifiedExtraction)
            };
            (reason, usage_shape(&value))
        }
        Err(_) => (UsageRejection::InvalidJson, Value::Null),
    };
    tracing::warn!(
        %request_id,
        stage = "buffered_usage_extraction",
        upstream_status = status,
        response_bytes = body.len(),
        rejection_reason = ?reason,
        usage_shape = %shape,
        "upstream usage rejected"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn rejection_reasons_preserve_checked_usage_failures() {
        for (value, reason) in [
            (
                serde_json::json!({"usage": []}),
                UsageRejection::InvalidType("usage"),
            ),
            (
                serde_json::json!({"usage": {"prompt_tokens": "private-value"}}),
                UsageRejection::InvalidType("prompt_tokens"),
            ),
            (
                serde_json::json!({"usage": {"prompt_cache_hit_tokens": -1}}),
                UsageRejection::OutOfRange("prompt_cache_hit_tokens"),
            ),
            (
                serde_json::json!({"usage": {"output_tokens": 2, "prompt_cache_hit_tokens": 1}}),
                UsageRejection::MissingPromptTokens,
            ),
            (
                serde_json::json!({"usage": {"input_tokens": 9, "prompt_tokens": 10, "prompt_cache_hit_tokens": 6}}),
                UsageRejection::InvalidPromptAlias,
            ),
            (
                serde_json::json!({"usage": {"prompt_tokens": 10, "prompt_tokens_details": {"cached_tokens": false}}}),
                UsageRejection::InvalidCachedType("prompt_tokens_details"),
            ),
            (
                serde_json::json!({"usage": {"prompt_tokens": 10, "prompt_cache_hit_tokens": 6, "prompt_tokens_details": {"cached_tokens": 5}}}),
                UsageRejection::ConflictingCacheAliases,
            ),
            (
                serde_json::json!({"usage": {"prompt_tokens": 10, "prompt_cache_hit_tokens": 6, "prompt_cache_miss_tokens": 5}}),
                UsageRejection::CacheNonconservation,
            ),
            (
                serde_json::json!({"usage": {"input_tokens": 1, "cache_creation": {"ephemeral_5m_input_tokens": i64::MAX, "ephemeral_1h_input_tokens": 1}}}),
                UsageRejection::CacheWriteOverflow,
            ),
            (
                serde_json::json!({"usage": {"prompt_tokens": i64::MIN, "cache_read_input_tokens": 1}}),
                UsageRejection::InputArithmeticOverflow,
            ),
            (
                serde_json::json!({"usage": {"output_tokens": -1}}),
                UsageRejection::OutOfRange("normalized_output_tokens"),
            ),
            (
                serde_json::json!({"usage": {"prompt_tokens": 1}, "service_tier": false}),
                UsageRejection::InvalidType("service_tier"),
            ),
            (
                serde_json::json!({"usage": {"prompt_tokens": 1}, "service_tier": "private-tier"}),
                UsageRejection::UnsupportedServiceTier,
            ),
        ] {
            assert_eq!(usage_from_value_diagnosed(&value).unwrap_err(), reason);
            assert!(usage_from_value_checked(&value).is_err());
            assert!(matches!(
                extract_buffered_usage_checked(
                    &serde_json::to_vec(&value).unwrap(),
                    "http-json",
                    Protocol::OpenAiChat
                ),
                ExtractedUsage::Invalid
            ));
        }
    }

    #[test]
    fn safe_shape_is_bounded_and_follows_usage_precedence() {
        let mut value = serde_json::json!({
            "usage": {"prompt_tokens": 10, "completion_tokens": "private-token", "prompt_tokens_details": {"cached_tokens": null}},
            "choices": [{"message": {"content": "private-output"}}],
            "error": {"message": "private-error"},
            "service_tier": "private-tier",
            "message": {"usage": {"input_tokens": 999}},
            "private-field-name": "private-field-value"
        });
        value["usage"]["private-nested-name"] = Value::String("private".repeat(100_000));
        let shape = usage_shape(&value);
        assert_eq!(shape["location"], "usage");
        assert_eq!(shape["prompt_tokens"]["count"], 10);
        assert_eq!(
            shape["completion_tokens"],
            serde_json::json!({"type": "string"})
        );
        assert_eq!(shape["prompt_cached_tokens"]["type"], "null");
        assert_eq!(shape["input_tokens"]["type"], "absent");
        assert_eq!(shape["choices"]["type"], "array");
        assert_eq!(shape["error"]["type"], "object");
        let encoded = shape.to_string();
        assert!(encoded.len() < 4096);
        assert!(!encoded.contains("private"));
        value["usage"] = Value::Null;
        assert_eq!(usage_shape(&value)["usage"]["type"], "null");
        assert_eq!(usage_shape(&value)["input_tokens"]["type"], "absent");
        value.as_object_mut().unwrap().remove("usage");
        assert_eq!(usage_shape(&value)["location"], "message.usage");
        assert_eq!(usage_shape(&value)["input_tokens"]["count"], 999);
        value["error"] = Value::from(987654321);
        value["choices"] = Value::from(987654321);
        value["service_tier"] = Value::from(987654321);
        assert!(!usage_shape(&value).to_string().contains("987654321"));
    }

    #[derive(Clone, Default)]
    struct Writer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn rejection_log_contains_only_correlated_safe_fields() {
        let writer = Writer::default();
        let sink = writer.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_writer(move || sink.clone())
            .finish();
        let request_id = Uuid::new_v4();
        let body = br#"{"usage":{"prompt_tokens":"private-token","completion_tokens":2},"choices":[{"message":{"content":"private-output"}}],"private-field":"private-value"}"#;
        tracing::subscriber::with_default(subscriber, || {
            observe_buffered_usage_rejection(
                request_id,
                200,
                body,
                "http-json",
                Protocol::OpenAiChat,
            );
            observe_buffered_usage_rejection(
                request_id,
                200,
                b"private-invalid-json",
                "http-json",
                Protocol::OpenAiChat,
            );
            observe_buffered_usage_rejection(
                request_id,
                200,
                body,
                crate::oauth::managed::kimi::PROVIDER_DRIVER,
                Protocol::OpenAiChat,
            );
        });
        let bytes = writer.0.lock().unwrap();
        let rendered = std::str::from_utf8(&bytes).unwrap();
        assert!(!rendered.contains("private"));
        let events: Vec<Value> = rendered
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 3);
        for event in &events {
            let fields = event["fields"].as_object().unwrap();
            assert_eq!(fields["request_id"], request_id.to_string());
            assert_eq!(fields["stage"], "buffered_usage_extraction");
            assert_eq!(fields["upstream_status"], 200);
            for field in fields.keys() {
                assert!(
                    [
                        "message",
                        "request_id",
                        "stage",
                        "upstream_status",
                        "response_bytes",
                        "rejection_reason",
                        "usage_shape"
                    ]
                    .contains(&field.as_str())
                );
            }
        }
        assert_eq!(
            events[0]["fields"]["rejection_reason"],
            "InvalidType(\"prompt_tokens\")"
        );
        assert_eq!(events[0]["fields"]["response_bytes"], body.len());
        assert_eq!(events[1]["fields"]["rejection_reason"], "InvalidJson");
        assert_eq!(
            events[2]["fields"]["rejection_reason"],
            "ProviderNormalization"
        );
    }
}
