use futures_util::StreamExt;
use http::header;
use serde_json::Value;
use uuid::Uuid;

use super::super::upstream_response::UpstreamResponse;

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
        error_code: diagnostic_machine_value(
            error.get("code"),
            &[
                "invalid_value",
                "invalid_type",
                "missing_required_parameter",
                "model_not_found",
                "unsupported_parameter",
                "invalid_request_error",
            ],
        ),
        error_param,
        reason: diagnostic_reason(message, error_param),
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
    let lower = message.unwrap_or_default().to_ascii_lowercase();
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
