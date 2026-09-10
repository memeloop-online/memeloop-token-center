use futures_util::StreamExt;
use http::header;
use serde_json::Value;

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
        Ok(BoundedBadRequestBody::Value(_)) => BadRequestDisposition::DefiniteOrdinary,
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
