use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use http::{HeaderMap, StatusCode, header};
use serde_json::{Value, json};

use super::upstream_response::UpstreamResponse;
use crate::{model::RequestTerminalCause, supplier_error::SafeReason};

pub(super) const MAX_ERROR_BYTES: usize = 16 * 1024;
const MAX_ERROR_WAIT: Duration = Duration::from_secs(5);

pub(in crate::api) struct Rejection {
    pub(in crate::api) body: Bytes,
    pub(super) terminal_cause: Option<RequestTerminalCause>,
}

// The terminal response owns submission/settlement semantics. Attach the
// diagnostic without replacing an uncertain or replayed terminal response.
// Only generated media terminal JSON is passed here, never the supplier stream.
pub(in crate::api) async fn with_media_rejection(
    response: axum::response::Response,
    rejection: Bytes,
) -> Result<axum::response::Response, crate::error::AppError> {
    let (mut parts, body) = response.into_parts();
    let original = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|_| crate::error::AppError::internal("media_terminal_body"))?;
    let mut value: Value = serde_json::from_slice(&original)
        .map_err(|_| crate::error::AppError::internal("media_terminal_json"))?;
    let diagnostic: Value = serde_json::from_slice(&rejection)
        .map_err(|_| crate::error::AppError::internal("media_rejection_json"))?;
    if let Some(object) = value.as_object_mut() {
        object.insert("provider_error".to_owned(), diagnostic);
    }
    let body = serde_json::to_vec(&value)
        .map_err(|_| crate::error::AppError::internal("media_terminal_encode"))?;
    parts.headers.remove(header::CONTENT_LENGTH);
    Ok(axum::response::Response::from_parts(
        parts,
        axum::body::Body::from(body),
    ))
}

pub(super) fn rejection_content_class(headers: &HeaderMap) -> &'static str {
    let mut content_types = headers.get_all(header::CONTENT_TYPE).iter();
    let first = content_types.next();
    if content_types.next().is_some() {
        return "ambiguous";
    }
    let Some(first) = first else {
        return "missing";
    };
    let Some(value) = first
        .to_str()
        .ok()
        .and_then(|value| value.split(';').next())
    else {
        return "other";
    };
    let value = value.trim().to_ascii_lowercase();
    if value == "application/json"
        || (value.starts_with("application/") && value.ends_with("+json"))
    {
        "json"
    } else if value == "text/html" {
        "html"
    } else {
        "other"
    }
}

pub(super) fn fallback_body() -> Bytes {
    Bytes::from_static(
        b"{\"error\":{\"message\":\"upstream rejected the request\",\"type\":\"upstream_error\"}}",
    )
}

pub(in crate::api) fn safe_error_body(raw: &[u8]) -> Bytes {
    if raw.is_empty() {
        return fallback_body();
    }
    let value = crate::api::sse::parse_unique_json(raw)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(raw).into_owned()));
    let envelope = json!({"error": crate::supplier_error::upstream_error_value(&value)});
    serde_json::to_vec(&envelope)
        .map(Bytes::from)
        .unwrap_or_else(|_| fallback_body())
}

// A model lookup rejection is execution evidence only when its complete,
// unique JSON envelope contains an explicit provider code and no result/usage.
// Human messages and our sanitized display text never authorize replay.
fn model_rejected_before_execution(raw: &[u8]) -> bool {
    let Ok(value) = crate::api::sse::parse_unique_json(raw) else {
        return false;
    };
    let Some(root) = value.as_object() else {
        return false;
    };
    let error = match root.get("error") {
        Some(Value::Object(error)) if root.len() == 1 => error,
        None => root,
        _ => return false,
    };
    if error.keys().any(|key| {
        !matches!(
            key.as_str(),
            "code" | "type" | "message" | "msg" | "param" | "status"
        )
    }) {
        return false;
    }
    if error
        .values()
        .any(|value| value.is_object() || value.is_array())
    {
        return false;
    }
    // A type-only/general message is insufficient: the provider must explicitly
    // reject model resolution, rather than report an interrupted generation.
    let Some(code) = error.get("code").and_then(Value::as_str) else {
        return false;
    };
    matches!(
        code.to_ascii_lowercase().as_str(),
        "model_not_found" | "unsupported_model" | "model_unavailable"
    )
}

/// Inspect using the existing error-reader budget, retaining every consumed
/// chunk for delivery/archive. Over-budget, partial and non-JSON bodies retain
/// their original stream and remain non-replayable.
pub(super) async fn inspect_model_rejection(
    response: UpstreamResponse,
) -> (UpstreamResponse, bool) {
    if !matches!(
        response.status(),
        StatusCode::BAD_REQUEST
            | StatusCode::NOT_FOUND
            | StatusCode::UNPROCESSABLE_ENTITY
            | StatusCode::SERVICE_UNAVAILABLE
    ) || rejection_content_class(response.headers()) != "json"
        || response
            .headers()
            .get_all(header::CONTENT_ENCODING)
            .iter()
            .any(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
        || response
            .content_length()
            .is_some_and(|length| length == 0 || length > MAX_ERROR_BYTES as u64)
    {
        return (response, false);
    }
    // A prefetched EOF must remain safe for the later archive/delivery reader.
    let mut parts = response.into_parts();
    parts.stream = Box::pin(parts.stream.fuse());
    let mut prefix = Vec::new();
    let mut body = Vec::new();
    let read = async {
        for _ in 0..64 {
            match parts.stream.next().await {
                Some(Ok(chunk)) => {
                    let fits = chunk.len() <= MAX_ERROR_BYTES.saturating_sub(body.len());
                    if fits {
                        body.extend_from_slice(&chunk);
                    }
                    prefix.push(Ok(chunk));
                    if !fits {
                        return false;
                    }
                }
                Some(Err(error)) => {
                    prefix.push(Err(error));
                    return false;
                }
                None => return true,
            }
        }
        false
    };
    let complete = tokio::time::timeout(MAX_ERROR_WAIT, read)
        .await
        .unwrap_or(false);
    let rejected = complete
        && parts
            .content_length
            .is_none_or(|length| length == body.len() as u64)
        && model_rejected_before_execution(&body);
    (
        UpstreamResponse::Prefetched {
            status: parts.status,
            headers: parts.headers,
            version: parts.version,
            content_length: parts.content_length,
            stream: Box::pin(futures_util::stream::iter(prefix).chain(parts.stream)),
        },
        rejected,
    )
}

pub(in crate::api) async fn read_rejection(response: UpstreamResponse) -> Rejection {
    let status = response.status();
    if status.is_success() {
        drop(response);
        return Rejection {
            body: fallback_body(),
            terminal_cause: (!status.is_success())
                .then_some(RequestTerminalCause::UpstreamHttpStatus),
        };
    }
    read_response_diagnostic_preserving(response).await.0
}

/// Diagnostic preview only. Return every consumed byte and the unread tail for
/// the existing raw archive owner; preview bounds never bound the archive.
pub(super) async fn read_response_diagnostic_preserving(
    response: UpstreamResponse,
) -> (Rejection, UpstreamResponse) {
    let status = response.status();
    let mut parts = response.into_parts();
    parts.stream = Box::pin(parts.stream.fuse());
    let mut prefetched = Vec::new();
    let mut body = Vec::new();
    let read = async {
        loop {
            match parts.stream.next().await {
                Some(Ok(chunk)) if chunk.len() <= MAX_ERROR_BYTES.saturating_sub(body.len()) => {
                    prefetched.push(Ok(chunk.clone()));
                    body.extend_from_slice(&chunk);
                    if chunk.is_empty() {
                        tokio::task::yield_now().await;
                    }
                }
                Some(Ok(chunk)) => {
                    prefetched.push(Ok(chunk.clone()));
                    body.extend_from_slice(&chunk[..MAX_ERROR_BYTES.saturating_sub(body.len())]);
                    body.extend_from_slice(
                        b"\n[MTC: upstream error body truncated at 16384 bytes]",
                    );
                    return (safe_error_body(&body), None);
                }
                Some(Err(error)) => {
                    prefetched.push(Err(error));
                    return (
                        safe_error_body(&body),
                        super::upstream_response::terminal_cause_from_stream_error(error),
                    );
                }
                None => return (safe_error_body(&body), None),
            }
        }
    };
    let (body, terminal_cause) = tokio::time::timeout(MAX_ERROR_WAIT, read)
        .await
        .unwrap_or_else(|_| {
            body.extend_from_slice(
                b"\n[MTC: timed out reading upstream error body after 5 seconds]",
            );
            (safe_error_body(&body), None)
        });
    let rejection = Rejection {
        body,
        terminal_cause: terminal_cause.or(Some(RequestTerminalCause::UpstreamHttpStatus)),
    };
    let original = UpstreamResponse::Prefetched {
        status: parts.status,
        headers: parts.headers,
        version: parts.version,
        content_length: parts.content_length,
        stream: Box::pin(futures_util::stream::iter(prefetched).chain(parts.stream)),
    };
    (rejection, original)
}

impl Rejection {
    pub(in crate::api) fn reason(&self) -> Option<SafeReason> {
        let value = crate::api::sse::parse_unique_json(&self.body).ok()?;
        crate::supplier_error::reason_from_value(&value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use http::{HeaderMap, HeaderValue, Version};

    #[tokio::test]
    async fn media_diagnostic_keeps_uncertain_terminal_and_provider_fields() {
        let response = axum::response::Response::builder()
            .status(StatusCode::ACCEPTED)
            .header("x-request-id", "media-request")
            .body(axum::body::Body::from(
                r#"{"error":{"code":"submission_unknown"},"retryable":false}"#,
            ))
            .unwrap();
        let diagnostic = safe_error_body(br#"{"error":{"code":"vendor_new_code","message":"specific explanation","extra":{"field":7}}}"#);
        let response = with_media_rejection(response, diagnostic).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(response.headers()["x-request-id"], "media-request");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], "submission_unknown");
        assert_eq!(value["retryable"], false);
        assert!(
            value["provider_error"]
                .to_string()
                .contains("specific explanation")
        );
        assert!(
            value["provider_error"]
                .to_string()
                .contains("vendor_new_code")
        );
        assert!(value["provider_error"].to_string().contains("field"));
    }

    #[tokio::test]
    async fn delayed_json_rejection_preserves_policy_reason_including_502() {
        for status in [StatusCode::BAD_REQUEST, StatusCode::BAD_GATEWAY] {
            let response = response(
                status,
                "application/json",
                None,
                Box::pin(stream::once(async {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    Ok(Bytes::from_static(br#"{"error":{"code":"misalignment_policy_violation","message":"private prompt"}}"#))
                })),
            );
            let rejection = read_rejection(response).await;
            assert_eq!(
                rejection.reason(),
                Some(SafeReason::MisalignmentPolicyViolation)
            );
            assert!(String::from_utf8_lossy(&rejection.body).contains("private prompt"));
        }
    }

    #[tokio::test]
    async fn diagnostic_preview_preserves_full_original_chunks_and_repeated_eof() {
        let first = Bytes::from(vec![b'x'; MAX_ERROR_BYTES + 1]);
        let tail = Bytes::from_static(b"complete-provider-tail");
        let (preview, original) = read_response_diagnostic_preserving(response(
            StatusCode::BAD_GATEWAY,
            "text/plain",
            None,
            Box::pin(stream::iter([Ok(first.clone()), Ok(tail.clone())])),
        ))
        .await;
        assert!(String::from_utf8_lossy(&preview.body).contains("truncated"));
        let mut original = original.bytes_stream();
        assert_eq!(original.next().await, Some(Ok(first)));
        assert_eq!(original.next().await, Some(Ok(tail)));
        assert_eq!(original.next().await, None);
        assert_eq!(original.next().await, None);
    }

    #[test]
    fn model_replay_requires_explicit_code_and_no_execution_evidence() {
        for raw in [
            r#"{"error":{"code":"model_not_found","message":"private arbitrary detail"}}"#,
            r#"{"code":"MODEL_NOT_FOUND","message":"arbitrary detail"}"#,
            r#"{"error":{"code":"unsupported_model"}}"#,
            r#"{"error":{"code":"model_unavailable"}}"#,
        ] {
            assert!(model_rejected_before_execution(raw.as_bytes()), "{raw}");
        }
        for raw in [
            r#"{"error":{"message":"The requested upstream model is unavailable"}}"#,
            r#"{"error":{"code":"service_unavailable"}}"#,
            r#"{"error":{"code":"model_not_found"},"usage":{"total_tokens":0}}"#,
            r#"{"error":{"code":"model_not_found","usage":null}}"#,
            r#"{"error":{"code":"model_not_found"},"output":[]}"#,
            r#"{"error":{"code":"model_not_found"},"choices":[{"text":"already generated"}]}"#,
            r#"{"error":{"code":"model_not_found","param":{"output":"generated"}}}"#,
            r#"{"error":{"code":"model_not_found","code":"service_unavailable"}}"#,
            r#"{"error":{"code":"model_not_found"}"#,
        ] {
            assert!(!model_rejected_before_execution(raw.as_bytes()), "{raw}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn model_rejection_inspection_preserves_wire_and_does_not_reject_over_budget_bodies() {
        let raw = Bytes::from_static(br#"{"error":{"code":"MODEL_NOT_FOUND"}}"#);
        for (status, content_type, length, chunks, expected) in [
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "application/json",
                Some(raw.len() as u64),
                vec![Ok(raw.clone())],
                true,
            ),
            (
                StatusCode::NOT_FOUND,
                "application/problem+json",
                None,
                vec![Ok(raw.slice(..10)), Ok(raw.slice(10..))],
                true,
            ),
            (
                StatusCode::OK,
                "application/json",
                None,
                vec![Ok(raw.clone())],
                false,
            ),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "text/event-stream",
                None,
                vec![Ok(raw.clone())],
                false,
            ),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "application/json",
                Some(raw.len() as u64 + 1),
                vec![Ok(raw.clone())],
                false,
            ),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "application/json",
                None,
                vec![Ok(raw.clone()), Err("upstream_stream_read_error")],
                false,
            ),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "application/json",
                None,
                vec![
                    Ok(Bytes::from(vec![b'x'; MAX_ERROR_BYTES + 1])),
                    Ok(raw.clone()),
                ],
                false,
            ),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "application/json",
                Some(MAX_ERROR_BYTES as u64 + 1),
                vec![Ok(raw.clone())],
                false,
            ),
        ] {
            let expected_chunks = chunks.clone();
            let (inspected, rejected) = inspect_model_rejection(response(
                status,
                content_type,
                length,
                Box::pin(stream::iter(chunks)),
            ))
            .await;
            assert_eq!(rejected, expected);
            assert_eq!(inspected.status(), status);
            assert_eq!(inspected.content_length(), length);
            assert_eq!(
                inspected.bytes_stream().collect::<Vec<_>>().await,
                expected_chunks
            );
        }
    }

    #[tokio::test]
    async fn inspected_non_fused_eof_can_be_delivered_and_archived_again() {
        for (raw, expected_rejection) in [
            (r#"{"error":{"code":"model_not_found"}}"#, true),
            (
                r#"{"error":{"code":"model_not_found"},"usage":{"total_tokens":1}}"#,
                false,
            ),
        ] {
            let body = Bytes::from_static(raw.as_bytes());
            // Unfold panics if polled after its first None, like the timeout
            // wrapper used by real HTTP upstream responses.
            let upstream = stream::unfold(Some(body.clone()), |body| async move {
                body.map(|bytes| (Ok(bytes), None))
            });
            let (inspected, rejected) = inspect_model_rejection(response(
                StatusCode::SERVICE_UNAVAILABLE,
                "application/json",
                Some(body.len() as u64),
                Box::pin(upstream),
            ))
            .await;
            assert_eq!(rejected, expected_rejection);
            let mut replay = inspected.into_parts().stream;
            assert_eq!(replay.next().await, Some(Ok(body)));
            assert_eq!(replay.next().await, None);
            assert_eq!(replay.next().await, None);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn partial_model_rejection_times_out_without_losing_consumed_prefix() {
        let prefix = Bytes::from_static(br#"{"error":{"code":"model_not_found"}}"#);
        let upstream = stream::iter([Ok(prefix.clone())]).chain(stream::pending());
        let started = tokio::time::Instant::now();
        let (inspected, rejected) = inspect_model_rejection(response(
            StatusCode::SERVICE_UNAVAILABLE,
            "application/json",
            None,
            Box::pin(upstream),
        ))
        .await;
        assert!(!rejected);
        assert_eq!(tokio::time::Instant::now() - started, MAX_ERROR_WAIT);
        let mut retained = inspected.bytes_stream();
        assert_eq!(retained.next().await, Some(Ok(prefix)));
        assert!(
            tokio::time::timeout(MAX_ERROR_WAIT, retained.next())
                .await
                .is_err()
        );
    }

    #[test]
    fn rejection_header_classes_are_fixed_and_share_the_reader_eligibility_rule() {
        let mut headers = HeaderMap::new();
        assert_eq!(rejection_content_class(&headers), "missing");
        for (value, expected) in [
            ("application/json; charset=utf-8", "json"),
            ("application/problem+json", "json"),
            ("text/html", "html"),
            ("private-header-canary", "other"),
        ] {
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(value));
            assert_eq!(rejection_content_class(&headers), expected);
        }
        headers.append(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        assert_eq!(rejection_content_class(&headers), "ambiguous");
    }
    use http::Version;

    fn response(
        status: StatusCode,
        content_type: &str,
        stream: super::super::upstream_response::UpstreamByteStream,
    ) -> UpstreamResponse {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, content_type.parse().unwrap());
        UpstreamResponse::Prefetched {
            status,
            headers,
            version: Version::HTTP_11,
            content_length: None,
            stream,
        }
    }

    #[tokio::test]
    async fn delayed_known_and_unknown_errors_retain_actual_explanation_even_for_502() {
        for code in ["misalignment_policy_violation", "new_upstream_error"] {
            for status in [StatusCode::BAD_REQUEST, StatusCode::BAD_GATEWAY] {
                let raw = serde_json::to_vec(
                    &json!({"error":{"code":code,"message":"Specific provider explanation"}}),
                )
                .unwrap();
                let rejection = read_rejection(response(
                    status,
                    "application/json",
                    Box::pin(stream::once(async move {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        Ok(Bytes::from(raw))
                    })),
                ))
                .await;
                let value: Value = serde_json::from_slice(&rejection.body).unwrap();
                assert_eq!(value["error"]["mtc_provider_code"], code);
                assert_eq!(value["error"]["message"], "Specific provider explanation");
                assert_eq!(
                    rejection.terminal_cause,
                    Some(RequestTerminalCause::UpstreamHttpStatus)
                );
            }
        }
    }

    #[tokio::test]
    async fn plain_text_html_and_malformed_json_remain_visible_as_text() {
        for (content_type, raw) in [
            ("text/plain", "proxy refused connection"),
            ("text/html", "<html>upstream connection failed</html>"),
            ("application/json", "{incomplete json"),
        ] {
            let rejection = read_rejection(response(
                StatusCode::BAD_GATEWAY,
                content_type,
                Box::pin(stream::iter([Ok(Bytes::from(raw))])),
            ))
            .await;
            let value: Value = serde_json::from_slice(&rejection.body).unwrap();
            assert_eq!(value["error"]["message"], raw);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn partial_body_survives_error_read_timeout_with_explicit_explanation() {
        let started = tokio::time::Instant::now();
        let chunks =
            stream::iter([Ok(Bytes::from_static(b"upstream overloaded"))]).chain(stream::pending());
        let rejection = read_rejection(response(
            StatusCode::SERVICE_UNAVAILABLE,
            "text/plain",
            Box::pin(chunks),
        ))
        .await;
        let value: Value = serde_json::from_slice(&rejection.body).unwrap();
        let message = value["error"]["message"].as_str().unwrap();
        assert!(message.contains("upstream overloaded"));
        assert!(message.contains("timed out reading upstream error body"));
        assert_eq!(tokio::time::Instant::now() - started, MAX_ERROR_WAIT);
        assert_eq!(
            rejection.terminal_cause,
            Some(RequestTerminalCause::UpstreamHttpStatus)
        );
    }

    #[tokio::test]
    async fn oversized_body_preserves_prefix_and_explicit_truncation() {
        let rejection = read_rejection(response(
            StatusCode::BAD_GATEWAY,
            "text/plain",
            Box::pin(stream::iter([Ok(Bytes::from(vec![
                b'x';
                MAX_ERROR_BYTES + 1
            ]))])),
        ))
        .await;
        let value: Value = serde_json::from_slice(&rejection.body).unwrap();
        let message = value["error"]["message"].as_str().unwrap();
        assert!(message.starts_with(&"x".repeat(MAX_ERROR_BYTES)));
        assert!(message.ends_with("[MTC: upstream error body truncated at 16384 bytes]"));
    }

    #[tokio::test(start_paused = true)]
    async fn transport_deadline_and_partial_evidence_survive() {
        let started = tokio::time::Instant::now();
        let rejection = read_rejection(
            response(
                StatusCode::BAD_GATEWAY,
                "text/plain",
                Box::pin(stream::pending()),
            )
            .with_body_timeouts(started + Duration::from_millis(20), Duration::from_secs(5)),
        )
        .await;
        assert_eq!(
            rejection.terminal_cause,
            Some(RequestTerminalCause::RequestTimeout)
        );
        assert_eq!(
            tokio::time::Instant::now() - started,
            Duration::from_millis(20)
        );
        let chunks = stream::iter([
            Ok(Bytes::from_static(b"proxy error details")),
            Err(super::super::upstream_response::UPSTREAM_HTTP2_RESET),
        ]);
        let rejection = read_rejection(response(
            StatusCode::BAD_GATEWAY,
            "text/plain",
            Box::pin(chunks),
        ))
        .await;
        assert_eq!(
            rejection.terminal_cause,
            Some(RequestTerminalCause::Http2Reset)
        );
        let value: Value = serde_json::from_slice(&rejection.body).unwrap();
        assert_eq!(value["error"]["message"], "proxy error details");
    }
}
