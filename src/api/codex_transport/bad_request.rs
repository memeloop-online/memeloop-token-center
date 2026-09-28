use futures_util::StreamExt;
use http::header;
use serde_json::Value;
use uuid::Uuid;

use super::super::upstream_response::UpstreamResponse;

#[path = "bad_request/non_json.rs"]
mod non_json;

const MAX_RETRYABLE_ERROR_BYTES: usize = 16 * 1024;
const MAX_RETRYABLE_ERROR_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Transport-domain result of inspecting a complete native Codex HTTP 400.
/// It contains no telemetry representation: routing owns the one-way map to
/// fixed metric labels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::api) enum BadRequestDisposition {
    DefiniteTransient,
    DefiniteOrdinary,
    Unclassifiable(BadRequestUnclassifiableReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::api) enum BadRequestUnclassifiableReason {
    ContentType,
    TooLarge,
    TimedOut,
    ReadFailed,
    InvalidJson,
}

/// Classify the narrowly-defined subset of Codex 400 responses which are safe
/// to replay before downstream delivery. A complete, bounded JSON body without
/// known transient semantics is a definite ordinary rejection and must not be
/// replayed; bodies which cannot be inspected safely remain unclassifiable.
pub(in crate::api) async fn classify_bad_request(
    response: UpstreamResponse,
    request_id: Uuid,
) -> BadRequestDisposition {
    if response.status() == http::StatusCode::BAD_REQUEST
        && !has_single_json_content_type(&response)
    {
        // Do not let an untrusted body delay delivery of the upstream error.
        // The response is still classified conservatively and never replayed.
        non_json::observe(&response, request_id);
        return BadRequestDisposition::Unclassifiable(BadRequestUnclassifiableReason::ContentType);
    }
    let disposition = inspect_bad_request(response, request_id).await;
    // Ordinary JSON errors already emit their bounded diagnostic below. All
    // other exits must also be attributable to the request, without recording
    // untrusted headers, error bodies, or transport error strings.
    if let Some((classification, reason)) = classification_diagnostic(disposition) {
        tracing::warn!(
            %request_id,
            stage = "codex_upstream_bad_request",
            upstream_error_classification = classification,
            upstream_error_reason = reason,
            "Codex upstream rejected the request"
        );
    }
    disposition
}

fn classification_diagnostic(
    disposition: BadRequestDisposition,
) -> Option<(&'static str, &'static str)> {
    Some(match disposition {
        BadRequestDisposition::DefiniteOrdinary => return None,
        BadRequestDisposition::DefiniteTransient => ("transient", "known_transient"),
        BadRequestDisposition::Unclassifiable(reason) => (
            "unclassifiable",
            match reason {
                BadRequestUnclassifiableReason::ContentType => "content_type",
                BadRequestUnclassifiableReason::TooLarge => "too_large",
                BadRequestUnclassifiableReason::TimedOut => "timed_out",
                BadRequestUnclassifiableReason::ReadFailed => "read_failed",
                BadRequestUnclassifiableReason::InvalidJson => "invalid_json",
            },
        ),
    })
}

async fn inspect_bad_request(
    response: UpstreamResponse,
    request_id: Uuid,
) -> BadRequestDisposition {
    if response.status() != http::StatusCode::BAD_REQUEST {
        return BadRequestDisposition::Unclassifiable(BadRequestUnclassifiableReason::ContentType);
    }
    if !has_single_json_content_type(&response) {
        return BadRequestDisposition::Unclassifiable(BadRequestUnclassifiableReason::ContentType);
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RETRYABLE_ERROR_BYTES as u64)
    {
        return BadRequestDisposition::Unclassifiable(BadRequestUnclassifiableReason::TooLarge);
    }

    match tokio::time::timeout(MAX_RETRYABLE_ERROR_WAIT, read_bounded_body(response)).await {
        Ok(BoundedBadRequestBody::Value(value)) if codex_transient_error(&value) => {
            BadRequestDisposition::DefiniteTransient
        }
        Ok(BoundedBadRequestBody::Value(value)) => {
            observe_ordinary_bad_request(request_id, &value);
            BadRequestDisposition::DefiniteOrdinary
        }
        Ok(BoundedBadRequestBody::TooLarge) => {
            BadRequestDisposition::Unclassifiable(BadRequestUnclassifiableReason::TooLarge)
        }
        Ok(BoundedBadRequestBody::ReadFailed) => {
            BadRequestDisposition::Unclassifiable(BadRequestUnclassifiableReason::ReadFailed)
        }
        Ok(BoundedBadRequestBody::InvalidJson) => {
            BadRequestDisposition::Unclassifiable(BadRequestUnclassifiableReason::InvalidJson)
        }
        Err(_) => BadRequestDisposition::Unclassifiable(BadRequestUnclassifiableReason::TimedOut),
    }
}

#[derive(Debug, Eq, PartialEq)]
struct BadRequestDiagnostic {
    error_type: Option<&'static str>,
    error_code: Option<&'static str>,
    error_param: Option<&'static str>,
    reason: &'static str,
}

fn observe_ordinary_bad_request(request_id: Uuid, value: &Value) {
    let diagnostic = bad_request_diagnostic(value);
    tracing::warn!(
        %request_id,
        stage = "codex_upstream_bad_request",
        upstream_error_classification = "ordinary",
        upstream_error_type = diagnostic.error_type,
        upstream_error_code = diagnostic.error_code,
        upstream_error_param = diagnostic.error_param,
        upstream_error_reason = diagnostic.reason,
        "Codex upstream rejected the request"
    );
}

fn bad_request_diagnostic(value: &Value) -> BadRequestDiagnostic {
    let error = value.get("error").unwrap_or(value);
    let detail = value.get("detail");
    let detail_item = detail
        .and_then(Value::as_array)
        .and_then(|items| items.first());
    let raw_param = error
        .get("param")
        .or_else(|| detail_item.and_then(|item| item.get("loc")));
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| detail.and_then(Value::as_str))
        .or_else(|| value.get("message").and_then(Value::as_str))
        .or_else(|| {
            detail_item
                .and_then(|item| item.get("msg"))
                .and_then(Value::as_str)
        });
    let error_param = diagnostic_param(raw_param);
    let error_code = diagnostic_machine_value(
        error.get("code"),
        &[
            "invalid_value",
            "invalid_type",
            "missing_required_parameter",
            "model_not_found",
            "unsupported_parameter",
            "invalid_request_error",
            "invalid_encrypted_content",
        ],
    );
    BadRequestDiagnostic {
        error_type: diagnostic_machine_value(
            error
                .get("type")
                .or_else(|| detail_item.and_then(|item| item.get("type"))),
            &[
                "invalid_request_error",
                "invalid_request",
                "request_error",
                "invalid_type",
                "value_error",
                "missing",
                "json_invalid",
            ],
        ),
        error_code,
        error_param,
        reason: if error_code == Some("invalid_encrypted_content") {
            "encrypted_content_rejected"
        } else {
            diagnostic_reason(message, error_param)
        },
    }
}

fn diagnostic_machine_value(
    value: Option<&Value>,
    allowed: &[&'static str],
) -> Option<&'static str> {
    let value = value?.as_str()?;
    Some(
        allowed
            .iter()
            .copied()
            .find(|candidate| *candidate == value)
            .unwrap_or("unknown"),
    )
}

fn diagnostic_param(value: Option<&Value>) -> Option<&'static str> {
    let mut fields = Vec::new();
    match value? {
        Value::String(value) => fields.push(value.as_str()),
        Value::Array(values) => {
            fields.extend(values.iter().filter_map(Value::as_str));
        }
        _ => return Some("unknown"),
    }
    // Match explicit path components before the legacy broad shape buckets:
    // encrypted_content and tool_choice must not disappear into content/unknown.
    for known in [
        "client_metadata",
        "tool_choice",
        "encrypted_content",
        "include",
        "compaction_trigger",
    ] {
        if fields.iter().any(|field| {
            field
                .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
                .any(|component| component == known)
        }) {
            return Some(known);
        }
    }
    if fields.iter().any(|field| {
        field
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .any(|component| matches!(component, "tools" | "namespace" | "parameters"))
    }) || fields.as_slice() == ["strict"]
    {
        return Some("tool_schema");
    }
    if fields.iter().any(|field| field.contains("instruction")) {
        Some("instructions")
    } else if fields.iter().any(|field| field.contains("content")) {
        Some("content")
    } else if fields.iter().any(|field| field.contains("model")) {
        Some("model")
    } else if fields
        .iter()
        .any(|field| field.contains("input") || field.contains("message"))
    {
        Some("input")
    } else if fields.iter().any(|field| {
        ["stream", "store", "include", "parallel", "prompt_cache"]
            .iter()
            .any(|known| field.contains(known))
    }) {
        Some("request_options")
    } else {
        Some("unknown")
    }
}

fn diagnostic_reason(message: Option<&str>, param: Option<&str>) -> &'static str {
    match param {
        Some("client_metadata") => return "client_metadata_rejected",
        Some("tool_choice") => return "tool_choice_rejected",
        Some("encrypted_content") => return "encrypted_content_rejected",
        Some("include") => return "include_rejected",
        Some("compaction_trigger") => return "compaction_trigger_rejected",
        _ => {}
    }
    let lower = message.unwrap_or_default().to_ascii_lowercase();
    if let Some(reason) = tool_rejection_reason(&lower) {
        return reason;
    }
    if param == Some("tool_schema") {
        return "tool_schema_rejected";
    }
    if lower.contains("encrypted")
        && ["invalid", "decrypt", "verify", "verified", "mismatch"]
            .iter()
            .any(|term| lower.contains(term))
    {
        return "encrypted_content_rejected";
    }
    let structural_rejection = ["invalid", "unsupported", "expected", "required", "missing"]
        .iter()
        .any(|term| lower.contains(term));
    if lower.contains("instruction") && structural_rejection {
        "instructions_invalid"
    } else if lower.contains("content") && structural_rejection {
        "content_shape_invalid"
    } else if lower.contains("model") && structural_rejection {
        "model_rejected"
    } else if (lower.contains("input") || lower.contains("message")) && structural_rejection {
        "input_shape_invalid"
    } else {
        match param {
            Some("instructions") => "instructions_invalid",
            Some("content") => "content_shape_invalid",
            Some("model") => "model_rejected",
            Some("input") => "input_shape_invalid",
            _ => "unknown",
        }
    }
}

/// These categories describe provider wording, never infer account entitlement.
/// Keep exact not-enabled wording separate from unsupported/not-available.
fn tool_rejection_reason(message: &str) -> Option<&'static str> {
    let words: Vec<_> = message
        // Diagnostic keywords must not come from URL hostnames (e.g. .invalid)
        // or key=value payloads appended to an otherwise explicit error.
        .split_whitespace()
        .filter(|part| !part.contains("://") && !part.contains('='))
        .flat_map(|part| {
            part.split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        })
        .filter(|word| !word.is_empty())
        .collect();
    let collaboration = words.contains(&"collaboration");
    let namespace = words.contains(&"namespace");
    if !collaboration && !namespace {
        return None;
    }
    let exact_not_enabled = [
        &["collaboration", "is", "not", "enabled"][..],
        &["collaboration", "not", "enabled"][..],
        &["namespace", "is", "not", "enabled"][..],
        &["namespace", "not", "enabled"][..],
    ]
    .iter()
    .any(|phrase| words.windows(phrase.len()).any(|window| window == *phrase));
    // Structural wording wins when an error also mentions an enablement rule.
    if words
        .iter()
        .any(|word| matches!(*word, "invalid" | "required" | "missing"))
    {
        return Some(if collaboration {
            "collaboration_schema_invalid"
        } else {
            "tool_namespace_schema_invalid"
        });
    }
    if exact_not_enabled {
        return Some(if collaboration {
            "collaboration_not_enabled"
        } else {
            "tool_namespace_not_enabled"
        });
    }
    if words
        .iter()
        .any(|word| matches!(*word, "unsupported" | "reserved"))
        || [
            ["not", "supported"],
            ["not", "available"],
            ["not", "allowed"],
            ["only", "allowed"],
        ]
        .iter()
        .any(|phrase| words.windows(phrase.len()).any(|window| window == phrase))
    {
        return Some(if collaboration {
            "collaboration_unsupported"
        } else {
            "tool_namespace_unsupported"
        });
    }
    None
}

enum BoundedBadRequestBody {
    Value(Value),
    TooLarge,
    ReadFailed,
    InvalidJson,
}

async fn read_bounded_body(response: UpstreamResponse) -> BoundedBadRequestBody {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            return BoundedBadRequestBody::ReadFailed;
        };
        if chunk.len() > MAX_RETRYABLE_ERROR_BYTES.saturating_sub(body.len()) {
            return BoundedBadRequestBody::TooLarge;
        }
        body.extend_from_slice(&chunk);
    }
    match serde_json::from_slice(&body) {
        Ok(value) => BoundedBadRequestBody::Value(value),
        Err(_) => BoundedBadRequestBody::InvalidJson,
    }
}

fn has_single_json_content_type(response: &UpstreamResponse) -> bool {
    let mut values = response.headers().get_all(header::CONTENT_TYPE).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    value
        .to_str()
        .ok()
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .is_some_and(|value| {
            value.eq_ignore_ascii_case("application/json") || value.ends_with("+json")
        })
}

#[cfg(test)]
pub(super) fn codex_transient_error(value: &Value) -> bool {
    let Some(error) = value.get("error").and_then(Value::as_object) else {
        return false;
    };
    if error
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(is_explicit_transient_error_type)
    {
        return true;
    }
    error
        .get("message")
        .and_then(Value::as_str)
        .is_some_and(is_known_high_demand_message)
}

#[cfg(not(test))]
fn codex_transient_error(value: &Value) -> bool {
    let Some(error) = value.get("error").and_then(Value::as_object) else {
        return false;
    };
    error
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(is_explicit_transient_error_type)
        || error
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(is_known_high_demand_message)
}

fn is_explicit_transient_error_type(error_type: &str) -> bool {
    matches!(
        error_type,
        "server_error"
            | "internal_error"
            | "temporarily_unavailable"
            | "service_unavailable"
            | "overloaded_error"
            | "high_demand"
            | "high_demand_error"
    )
}

fn is_known_high_demand_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("high demand") || message.contains("high-demand")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tracing::instrument::WithSubscriber;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    async fn non_json_diagnostic(response: &UpstreamResponse) -> Value {
        let capture = Capture::default();
        let _other_dispatch = tracing::Dispatch::new(tracing_subscriber::registry());
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_writer(capture.clone())
            .finish();
        let disposition = classify_bad_request(response, Uuid::nil())
            .with_subscriber(subscriber)
            .await;
        assert_eq!(
            disposition,
            BadRequestDisposition::Unclassifiable(BadRequestUnclassifiableReason::ContentType)
        );
        let logged = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert_eq!(logged.lines().count(), 1);
        assert!(!logged.contains("private-canary"), "{logged}");
        let event: Value = serde_json::from_str(logged.trim()).unwrap();
        assert_eq!(
            event["fields"]["upstream_error_classification"],
            "unclassifiable"
        );
        assert_eq!(event["fields"]["upstream_error_reason"], "content_type");
        event["fields"].clone()
    }

    #[test]
    fn tool_schema_parameters_and_provider_wording_are_fixed_private_categories() {
        for param in [
            json!("tools[0].namespace"),
            json!("input[0].tools[1].parameters.private-canary"),
            json!(["body", "tools", 0, "strict"]),
        ] {
            let diagnostic = bad_request_diagnostic(&json!({
                "error": {"param": param, "message": "private-canary model rejected"}
            }));
            assert_eq!(diagnostic.error_param, Some("tool_schema"));
            assert_eq!(diagnostic.reason, "tool_schema_rejected");
            assert!(!format!("{diagnostic:?}").contains("private-canary"));
        }
        assert_eq!(
            diagnostic_param(Some(&json!("private_namespace_canary"))),
            Some("unknown")
        );
        assert_eq!(
            diagnostic_param(Some(&json!("strict"))),
            Some("tool_schema")
        );
        for param in [
            json!("response_format.json_schema.strict"),
            json!(["body", "text", "format", "strict"]),
        ] {
            assert_ne!(diagnostic_param(Some(&param)), Some("tool_schema"));
        }
        for (message, expected) in [
            ("Collaboration is not enabled", "collaboration_not_enabled"),
            (
                "Namespace 'collaboration' is not enabled",
                "collaboration_not_enabled",
            ),
            (
                "Collaboration is not available for this model",
                "collaboration_unsupported",
            ),
            (
                "Collaboration is not supported",
                "collaboration_unsupported",
            ),
            (
                "Collaboration is only allowed in a different mode",
                "collaboration_unsupported",
            ),
            ("Collaboration is reserved", "collaboration_unsupported"),
            (
                "Invalid collaboration parameters",
                "collaboration_schema_invalid",
            ),
            (
                "Required collaboration field missing; namespace is not enabled",
                "collaboration_schema_invalid",
            ),
            (
                "Tool namespace is not enabled",
                "tool_namespace_not_enabled",
            ),
            ("Unsupported tool namespace", "tool_namespace_unsupported"),
            ("Invalid tool namespace", "tool_namespace_schema_invalid"),
            ("Collaboration request rejected", "unknown"),
            (
                "Collaboration denied; another feature is not enabled",
                "unknown",
            ),
            ("A private_collaboration field was rejected", "unknown"),
        ] {
            let capture = Capture::default();
            let _other_dispatch = tracing::Dispatch::new(tracing_subscriber::registry());
            let subscriber = tracing_subscriber::fmt()
                .json()
                .without_time()
                .with_writer(capture.clone())
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                observe_ordinary_bad_request(
                    Uuid::nil(),
                    &json!({
                        "error": {
                            "message": format!("{message}; private-canary https://private-canary.invalid token=private-canary"),
                            "type": "private-canary",
                            "code": "private-canary",
                        }
                    }),
                );
            });
            let logged = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
            assert!(!logged.contains("private-canary"));
            let event: Value = serde_json::from_str(logged.trim()).unwrap();
            assert_eq!(
                event["fields"]["upstream_error_reason"], expected,
                "{message}"
            );
        }
    }

    #[tokio::test]
    async fn non_json_diagnostic_does_not_read_untrusted_body() {
        let fields = non_json_diagnostic(&UpstreamResponse::Prefetched {
            status: http::StatusCode::BAD_REQUEST,
            headers: http::HeaderMap::new(),
            version: http::Version::HTTP_2,
            content_length: None,
            stream: Box::pin(futures_util::stream::iter([Ok(bytes::Bytes::from_static(
                b"Collaboration is not enabled; private-canary",
            ))])),
        })
        .await;
        assert_eq!(fields["upstream_diagnostic_read"], "not_attempted");
        assert!(!fields.to_string().contains("private-canary"));
    }

    #[tokio::test(start_paused = true)]
    async fn every_unclassifiable_exit_emits_one_safe_request_diagnostic() {
        use BadRequestUnclassifiableReason::*;
        for (reason, label) in [
            (ContentType, "content_type"),
            (TooLarge, "too_large"),
            (TimedOut, "timed_out"),
            (ReadFailed, "read_failed"),
            (InvalidJson, "invalid_json"),
        ] {
            let mut headers = http::HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                http::HeaderValue::from_static(if reason == ContentType {
                    "text/html; private-header-canary"
                } else {
                    "application/json"
                }),
            );
            let stream: super::super::super::upstream_response::UpstreamByteStream =
                if reason == TimedOut {
                    Box::pin(futures_util::stream::pending())
                } else if reason == ReadFailed {
                    Box::pin(futures_util::stream::iter([Err(
                        "private-transport-canary",
                    )]))
                } else {
                    Box::pin(futures_util::stream::iter([Ok(bytes::Bytes::from_static(
                        b"private-body-canary not json",
                    ))]))
                };
            let response = UpstreamResponse::Prefetched {
                status: http::StatusCode::BAD_REQUEST,
                headers,
                version: http::Version::HTTP_2,
                content_length: (reason == TooLarge)
                    .then_some(MAX_RETRYABLE_ERROR_BYTES as u64 + 1),
                stream,
            };
            let capture = Capture::default();
            let _other_dispatch = tracing::Dispatch::new(tracing_subscriber::registry());
            let subscriber = tracing_subscriber::fmt()
                .json()
                .without_time()
                .with_writer(capture.clone())
                .finish();
            let request_id = Uuid::now_v7();
            let disposition = classify_bad_request(response, request_id)
                .with_subscriber(subscriber)
                .await;
            assert_eq!(disposition, BadRequestDisposition::Unclassifiable(reason));
            let logged = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
            assert_eq!(logged.lines().count(), 1, "{reason:?}: {logged}");
            assert!(!logged.contains("canary"), "{logged}");
            let event: Value = serde_json::from_str(logged.trim()).unwrap();
            assert_eq!(event["fields"]["request_id"], request_id.to_string());
            assert_eq!(event["fields"]["stage"], "codex_upstream_bad_request");
            assert_eq!(
                event["fields"]["upstream_error_classification"],
                "unclassifiable"
            );
            assert_eq!(event["fields"]["upstream_error_reason"], label);
        }
    }

    #[test]
    fn complex_request_parameters_keep_fixed_non_sensitive_diagnostics() {
        for (param, expected_param, reason) in [
            (
                "client_metadata",
                "client_metadata",
                "client_metadata_rejected",
            ),
            ("tool_choice", "tool_choice", "tool_choice_rejected"),
            (
                "input[0].encrypted_content",
                "encrypted_content",
                "encrypted_content_rejected",
            ),
            ("include[0]", "include", "include_rejected"),
            (
                "input[2].compaction_trigger",
                "compaction_trigger",
                "compaction_trigger_rejected",
            ),
        ] {
            for location in [json!(param), json!(["body", param])] {
                let diagnostic = bad_request_diagnostic(&json!({
                    "error": {
                        "type": "invalid_request_error",
                        "code": "unsupported_parameter",
                        "param": location,
                        "message": "Invalid input: private-body-canary https://private.invalid token=private-token"
                    }
                }));
                assert_eq!(diagnostic.error_param, Some(expected_param));
                assert_eq!(diagnostic.reason, reason);
                assert!(!format!("{diagnostic:?}").contains("private"));
            }
        }
        assert_eq!(
            diagnostic_param(Some(&json!("secret_tool_choice_secret"))),
            Some("unknown")
        );
    }

    #[test]
    fn encrypted_history_rejection_keeps_code_without_retaining_ciphertext() {
        for error in [
            json!({"code": "invalid_encrypted_content"}),
            json!({"message": "The encrypted content private-ciphertext could not be verified."}),
            json!({"param": "input[2].encrypted_content", "message": "private-ciphertext"}),
        ] {
            let diagnostic = bad_request_diagnostic(&json!({"error": error}));
            assert_eq!(diagnostic.reason, "encrypted_content_rejected");
            assert!(!format!("{diagnostic:?}").contains("private-ciphertext"));
            if error.get("code").is_some() {
                assert_eq!(diagnostic.error_code, Some("invalid_encrypted_content"));
            }
        }
    }

    #[test]
    fn ordinary_error_diagnostic_keeps_only_known_machine_fields_and_fixed_reason() {
        let diagnostic = bad_request_diagnostic(&json!({
            "error": {
                "type": "invalid_request_error",
                "code": "invalid_value",
                "param": "input[0].content",
                "message": "Invalid content: patient HIV Alice@example.com password=short"
            }
        }));
        assert_eq!(diagnostic.error_type, Some("invalid_request_error"));
        assert_eq!(diagnostic.error_code, Some("invalid_value"));
        assert_eq!(diagnostic.error_param, Some("content"));
        assert_eq!(diagnostic.reason, "content_shape_invalid");
        assert!(!format!("{diagnostic:?}").contains("patient HIV"));
        assert!(!format!("{diagnostic:?}").contains("Alice@example.com"));
        assert!(!format!("{diagnostic:?}").contains("password=short"));
    }

    #[test]
    fn arbitrary_machine_fields_and_free_text_are_never_logged_verbatim() {
        let diagnostic = bad_request_diagnostic(&json!({
            "error": {
                "type": "privateName",
                "code": "shortSecret",
                "param": "patientHIV",
                "message": "prompt rejected: patient HIV Alice@example.com"
            }
        }));
        assert_eq!(diagnostic.error_type, Some("unknown"));
        assert_eq!(diagnostic.error_code, Some("unknown"));
        assert_eq!(diagnostic.error_param, Some("unknown"));
        assert_eq!(diagnostic.reason, "unknown");
        let rendered = format!("{diagnostic:?}");
        for private in [
            "privateName",
            "shortSecret",
            "patientHIV",
            "patient HIV",
            "Alice@example.com",
        ] {
            assert!(!rendered.contains(private));
        }
    }

    #[test]
    fn detail_only_error_exposes_bounded_location_and_reason_template() {
        let diagnostic = bad_request_diagnostic(&json!({
            "detail": [{
                "loc": ["body", "input", 0, "content"],
                "msg": "Field required",
                "type": "missing"
            }]
        }));
        assert_eq!(diagnostic.error_type, Some("missing"));
        assert_eq!(diagnostic.error_param, Some("content"));
        assert_eq!(diagnostic.reason, "content_shape_invalid");
    }
}
