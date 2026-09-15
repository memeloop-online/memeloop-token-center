use futures_util::StreamExt;
use http::header;
use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;
use uuid::Uuid;

use super::super::upstream_response::UpstreamResponse;

const MAX_RETRYABLE_ERROR_BYTES: usize = 16 * 1024;
const MAX_RETRYABLE_ERROR_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
const MAX_DIAGNOSTIC_FIELD_BYTES: usize = 128;
const MAX_DIAGNOSTIC_REASON_BYTES: usize = 256;

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

#[derive(Debug, Default, Eq, PartialEq)]
struct BadRequestDiagnostic {
    error_type: Option<String>,
    error_code: Option<String>,
    error_param: Option<String>,
    reason: Option<String>,
}

fn observe_ordinary_bad_request(request_id: Uuid, value: &Value) {
    let diagnostic = bad_request_diagnostic(value);
    tracing::warn!(
        %request_id,
        stage = "codex_upstream_bad_request",
        upstream_error_type = diagnostic.error_type.as_deref(),
        upstream_error_code = diagnostic.error_code.as_deref(),
        upstream_error_param = diagnostic.error_param.as_deref(),
        upstream_error_reason = diagnostic.reason.as_deref(),
        "Codex upstream rejected the request"
    );
}

fn bad_request_diagnostic(value: &Value) -> BadRequestDiagnostic {
    let error = value.get("error").unwrap_or(value);
    let detail = value.get("detail");
    let detail_item = detail
        .and_then(Value::as_array)
        .and_then(|items| items.first());
    BadRequestDiagnostic {
        error_type: diagnostic_scalar(
            error
                .get("type")
                .or_else(|| detail_item.and_then(|item| item.get("type"))),
        ),
        error_code: diagnostic_scalar(error.get("code")),
        error_param: diagnostic_scalar(error.get("param")).or_else(|| {
            detail_item
                .and_then(|item| item.get("loc"))
                .and_then(diagnostic_location)
        }),
        reason: error
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| detail.and_then(Value::as_str))
            .or_else(|| value.get("message").and_then(Value::as_str))
            .or_else(|| {
                detail_item
                    .and_then(|item| item.get("msg"))
                    .and_then(Value::as_str)
            })
            .and_then(sanitize_diagnostic_reason),
    }
}

fn diagnostic_scalar(value: Option<&Value>) -> Option<String> {
    let value = match value? {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        _ => return None,
    };
    (!value.is_empty()
        && value.len() <= MAX_DIAGNOSTIC_FIELD_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/' | b'[' | b']')
        }))
    .then_some(value)
}

fn diagnostic_location(value: &Value) -> Option<String> {
    let parts = value.as_array()?;
    let mut rendered = String::new();
    for part in parts {
        let part = diagnostic_scalar(Some(part))?;
        if !rendered.is_empty() {
            rendered.push('.');
        }
        rendered.push_str(&part);
        if rendered.len() > MAX_DIAGNOSTIC_FIELD_BYTES {
            return None;
        }
    }
    (!rendered.is_empty()).then_some(rendered)
}

fn sanitize_diagnostic_reason(value: &str) -> Option<String> {
    static SENSITIVE: OnceLock<Regex> = OnceLock::new();
    let redacted = SENSITIVE
        .get_or_init(|| {
            Regex::new(
                r#"(?i:bearer)\s+\S+|https?://\S+|\"[^\"]*\"|'[^']*'|[A-Za-z0-9_./+=-]{32,}"#,
            )
            .expect("static diagnostic redaction regex")
        })
        .replace_all(value, "[redacted]");
    let normalized = redacted.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    let mut bounded = String::new();
    for character in normalized.chars() {
        if character.is_control() {
            continue;
        }
        if bounded.len() + character.len_utf8() > MAX_DIAGNOSTIC_REASON_BYTES {
            break;
        }
        bounded.push(character);
    }
    (!bounded.is_empty()).then_some(bounded)
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

    #[test]
    fn ordinary_error_diagnostic_keeps_structure_and_redacts_opaque_values() {
        let diagnostic = bad_request_diagnostic(&json!({
            "error": {
                "type": "invalid_request_error",
                "code": "invalid_value",
                "param": "input[0].content",
                "message": "Invalid \"private user text\" with Bearer secret-token-value and abcdefghijklmnopqrstuvwxyz0123456789"
            }
        }));
        assert_eq!(
            diagnostic.error_type.as_deref(),
            Some("invalid_request_error")
        );
        assert_eq!(diagnostic.error_code.as_deref(), Some("invalid_value"));
        assert_eq!(diagnostic.error_param.as_deref(), Some("input[0].content"));
        let reason = diagnostic.reason.unwrap();
        assert!(reason.contains("Invalid [redacted]"));
        assert!(!reason.contains("private user text"));
        assert!(!reason.contains("secret-token-value"));
        assert!(!reason.contains("abcdefghijklmnopqrstuvwxyz0123456789"));
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
        assert_eq!(diagnostic.error_type.as_deref(), Some("missing"));
        assert_eq!(
            diagnostic.error_param.as_deref(),
            Some("body.input.0.content")
        );
        assert_eq!(diagnostic.reason.as_deref(), Some("Field required"));
    }
}
