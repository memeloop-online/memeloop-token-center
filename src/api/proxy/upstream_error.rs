use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use http::{StatusCode, header};
use serde_json::{Value, json};

use super::upstream_response::UpstreamResponse;
use crate::{model::RequestTerminalCause, supplier_error::SafeReason};

const MAX_ERROR_BYTES: usize = 16 * 1024;
const MAX_ERROR_WAIT: Duration = Duration::from_millis(100);

pub(super) struct Rejection {
    pub(super) body: Bytes,
    pub(super) terminal_cause: Option<RequestTerminalCause>,
}

pub(super) fn fallback_body() -> Bytes {
    Bytes::from_static(
        b"{\"error\":{\"message\":\"upstream rejected the request\",\"type\":\"upstream_error\"}}",
    )
}

fn safe_error_body(raw: &[u8]) -> Bytes {
    if raw.len() > MAX_ERROR_BYTES {
        return fallback_body();
    }
    let Ok(value) = crate::api::sse::parse_unique_json(raw) else {
        return fallback_body();
    };
    let Some(object) = value.as_object() else {
        return fallback_body();
    };
    let error = match object.get("error") {
        Some(Value::Object(error)) => error,
        None => object,
        _ => return fallback_body(),
    };
    let message = match (error.get("message"), error.get("msg")) {
        (Some(_), Some(_)) => return fallback_body(),
        (Some(value), None) | (None, Some(value)) => value.as_str(),
        _ => None,
    };
    let numeric_code = error
        .get("code")
        .and_then(Value::as_u64)
        .and_then(|code| match code {
            400 => Some("400"),
            402 => Some("402"),
            _ => None,
        });
    let code = error.get("code").and_then(Value::as_str).or(numeric_code);
    let Some(reason) = SafeReason::from_supplier(code, message) else {
        return fallback_body();
    };
    let mut envelope = json!({"error": {
        "type": "upstream_error",
        "code": reason.code(),
        "message": reason.message(),
        "mtc_safe_reason": reason.code()
    }});
    let (code, message) = reason.provider_detail(code, message);
    if let Some(code) = code {
        envelope["error"]["mtc_provider_code"] = json!(code);
    }
    if let Some(message) = message {
        envelope["error"]["mtc_provider_message"] = json!(message);
    }
    serde_json::to_vec(&envelope)
        .map(Bytes::from)
        .unwrap_or_else(|_| fallback_body())
}

pub(super) async fn read_rejection(response: UpstreamResponse) -> Rejection {
    let status = response.status();
    if status == StatusCode::BAD_GATEWAY {
        return Rejection {
            body: fallback_body(),
            terminal_cause: super::upstream_response::rejected_response_terminal_cause(response)
                .await,
        };
    }
    let mut content_types = response.headers().get_all(header::CONTENT_TYPE).iter();
    let json_content = content_types
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| {
            let value = value.trim().to_ascii_lowercase();
            value == "application/json"
                || (value.starts_with("application/") && value.ends_with("+json"))
        })
        && content_types.next().is_none();
    let identity_encoding = response
        .headers()
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .all(|value| value.as_bytes().eq_ignore_ascii_case(b"identity"));
    if status.is_success()
        || !json_content
        || !identity_encoding
        || response
            .content_length()
            .is_some_and(|length| length == 0 || length > MAX_ERROR_BYTES as u64)
    {
        drop(response);
        return Rejection {
            body: fallback_body(),
            terminal_cause: None,
        };
    }
    let mut stream = response.bytes_stream();
    let read = async {
        let mut body = Vec::new();
        for _ in 0..64 {
            match stream.next().await {
                Some(Ok(chunk)) if chunk.len() <= MAX_ERROR_BYTES.saturating_sub(body.len()) => {
                    body.extend_from_slice(&chunk);
                }
                Some(Ok(_)) | Some(Err(_)) => return fallback_body(),
                None => return safe_error_body(&body),
            }
        }
        fallback_body()
    };
    let body = tokio::time::timeout(MAX_ERROR_WAIT, read)
        .await
        .unwrap_or_else(|_| fallback_body());
    Rejection {
        body,
        terminal_cause: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use http::{HeaderMap, HeaderValue, Version};

    fn response(
        status: StatusCode,
        content_type: &str,
        length: Option<u64>,
        stream: super::super::upstream_response::UpstreamByteStream,
    ) -> UpstreamResponse {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_str(content_type).unwrap(),
        );
        UpstreamResponse::Prefetched {
            status,
            headers,
            version: Version::HTTP_11,
            content_length: length,
            stream,
        }
    }

    #[test]
    fn only_explicit_codes_and_complete_known_messages_produce_fixed_reasons() {
        for (code, message, expected) in [
            (
                "NoAvailablePlan",
                "supplier private text",
                SafeReason::NoActivePlan,
            ),
            (
                "insufficient_quota",
                "private partial prompt",
                SafeReason::InsufficientQuota,
            ),
            (
                "invalid_api_key",
                "Bearer caller-canary",
                SafeReason::AuthenticationInvalid,
            ),
            (
                "api_key_expired",
                "private%20prompt",
                SafeReason::AuthenticationExpired,
            ),
            (
                "rate_limit_exceeded",
                "cookie=canary",
                SafeReason::RateLimited,
            ),
            (
                "unsupported_model",
                "private model",
                SafeReason::ModelUnavailable,
            ),
        ] {
            let body = safe_error_body(
                &serde_json::to_vec(&json!({"error": {
                "code": code, "message": message, "debug": "private-debug"
            }, "usage": {"prompt_tokens": 100}}))
                .unwrap(),
            );
            let value: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                value,
                json!({"error": {"type": "upstream_error",
                "code": expected.code(), "message": expected.message(), "mtc_safe_reason": expected.code(),
                "mtc_provider_code": code}})
            );
            let inline = format!("inline-json:{}", std::str::from_utf8(&body).unwrap());
            let projected =
                crate::supplier_error::supplier_error_from_inline_json(Some(&inline)).unwrap();
            assert_eq!(projected.code, expected.code());
            assert_eq!(
                projected.message,
                format!("{}; provider code: {code}", expected.message())
            );
        }
        let body = safe_error_body(r#"{"message":"当前账号没有可用套餐","code":402}"#.as_bytes());
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"]["code"], "no_active_plan");
        assert_eq!(value["error"]["message"], "当前账号没有可用套餐");
        assert_eq!(value["error"]["mtc_provider_code"], "402");
        assert_eq!(
            value["error"]["mtc_provider_message"],
            "当前账号没有可用套餐"
        );
        let model = safe_error_body(br#"{"error":{"code":400,"message":"Model not found","headers":{"Authorization":"Bearer private-canary"},"url":"https://private.invalid/?signature=private-canary"}}"#);
        let model: Value = serde_json::from_slice(&model).unwrap();
        assert_eq!(model["error"]["mtc_provider_code"], "400");
        assert_eq!(model["error"]["mtc_provider_message"], "Model not found");
        assert!(!model.to_string().contains("private-canary"));
        assert_eq!(
            safe_error_body(br#"{"error":{"code":"invalid_api_key","message":"No active plan"}}"#),
            fallback_body()
        );
    }

    #[test]
    fn full_partial_encoded_and_unrelated_private_text_is_never_reflected() {
        for text in [
            "Rejected selected-key-canary patient private-canary short-private https://private.invalid/path?token=query-canary token=unselected-canary sk-othercanary",
            "Rejected short%2Fkey short/\u{0007}key private promptline",
            "Rejected Authorization: Bearer unknown-canary",
            "Rejected Cookie: sid=unknown-cookie",
            "Rejected Bearer caller-canary sid=cookie-canary",
            "Rejected caller-canary cookie-canary",
            "Rejected private prompt https://private.invalid token=private-token",
            "Rejected api_key=Bearer unknown-canary",
            "private prompt full canary",
            "prompt full",
            "private%20prompt%20full%20canary",
            "pr%69vate+prompt",
            "provider confidential free text",
            "No active plan private-canary",
            "private-canary No active plan",
            "当前账号没有可用套餐 private-canary",
            "Unexpected supplier validation failure",
        ] {
            for value in [
                json!({"error": {"message": text, "code": "supplier_unknown"}}),
                json!({"error": {"message": "unknown rejection", "code": text}}),
            ] {
                assert_eq!(
                    safe_error_body(&serde_json::to_vec(&value).unwrap()),
                    fallback_body()
                );
            }
            let body = safe_error_body(
                &serde_json::to_vec(&json!({"error": {
                "code": "NoAvailablePlan", "message": text, "param": text
            }, "debug": text}))
                .unwrap(),
            );
            let value: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(value["error"]["message"], "当前账号没有可用套餐");
            assert_eq!(value["error"].as_object().unwrap().len(), 5);
            assert_eq!(value["error"]["mtc_provider_code"], "NoAvailablePlan");
            assert!(value["error"].get("mtc_provider_message").is_none());
            assert_eq!(value.as_object().unwrap().len(), 1);
            assert!(!String::from_utf8_lossy(&body).contains(text));
        }
    }

    #[test]
    fn invalid_ambiguous_unsafe_and_oversized_fields_fail_closed() {
        for body in [
            br#"{}"#.as_slice(),
            br#"{"error":"private"}"#,
            br#"{"error":{"message":"No active plan","message":"private"}}"#,
            br#"{"error":{"code":"NoAvailablePlan","code":"private"}}"#,
            br#"{"error":{"message":"No active plan","msg":"private"}}"#,
            br#"{"error":{"code":"NoAvailablePlan"},"debug":{"x":1,"x":2}}"#,
            br#"{"error":{"message":"<html>private</html>"}}"#,
            br#"{"error":{"message":"unterminated"#,
            br#"{"message":"unknown","code":402}"#,
        ] {
            assert_eq!(safe_error_body(body), fallback_body());
        }
        assert_eq!(
            safe_error_body(
                &serde_json::to_vec(&json!({"error": {"message": "x".repeat(2049)}})).unwrap()
            ),
            fallback_body()
        );
        assert_eq!(safe_error_body(&serde_json::to_vec(&json!({"error": {"code": "NoAvailablePlan", "message": "x".repeat(MAX_ERROR_BYTES)}})).unwrap()), fallback_body());
    }

    #[tokio::test(start_paused = true)]
    async fn non_json_empty_oversized_and_success_never_poll_the_body() {
        for (status, content_type, length) in [
            (StatusCode::PAYMENT_REQUIRED, "text/html", None),
            (StatusCode::PAYMENT_REQUIRED, "text/event-stream", None),
            (StatusCode::PAYMENT_REQUIRED, "application/json", Some(0)),
            (
                StatusCode::PAYMENT_REQUIRED,
                "application/json",
                Some((MAX_ERROR_BYTES + 1) as u64),
            ),
            (StatusCode::OK, "application/json", None),
        ] {
            let started = tokio::time::Instant::now();
            let body = stream::poll_fn(|_| panic!("ineligible body must not be polled"));
            let result =
                read_rejection(response(status, content_type, length, Box::pin(body))).await;
            assert_eq!(result.body, fallback_body());
            assert_eq!(tokio::time::Instant::now(), started);
        }
        for (name, value) in [
            (header::CONTENT_ENCODING, "gzip"),
            (header::CONTENT_TYPE, "application/json"),
        ] {
            let mut ineligible = response(
                StatusCode::PAYMENT_REQUIRED,
                "application/json",
                None,
                Box::pin(stream::poll_fn(|_| {
                    panic!("ineligible headers must not poll")
                })),
            );
            if let UpstreamResponse::Prefetched { headers, .. } = &mut ineligible {
                headers.append(name, HeaderValue::from_static(value));
            }
            assert_eq!(read_rejection(ineligible).await.body, fallback_body());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_json_has_one_short_budget_and_never_mints_a_timeout_cause() {
        let started = tokio::time::Instant::now();
        let result = read_rejection(response(
            StatusCode::PAYMENT_REQUIRED,
            "application/json",
            None,
            Box::pin(stream::pending()),
        ))
        .await;
        assert_eq!(result.body, fallback_body());
        assert_eq!(result.terminal_cause, None);
        assert_eq!(tokio::time::Instant::now() - started, MAX_ERROR_WAIT);
    }

    #[tokio::test(start_paused = true)]
    async fn complete_bounded_json_required_and_transport_deadline_remains_absolute() {
        let raw = serde_json::to_vec(
            &json!({"error": {"message": "当前账号没有可用套餐", "code": "NoPlan"}}),
        )
        .unwrap();
        let chunks = stream::iter(
            raw.chunks(8)
                .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                .collect::<Vec<_>>(),
        );
        let result = read_rejection(response(
            StatusCode::PAYMENT_REQUIRED,
            "application/json",
            None,
            Box::pin(chunks),
        ))
        .await;
        let safe: Value = serde_json::from_slice(&result.body).unwrap();
        assert_eq!(safe["error"]["message"], "当前账号没有可用套餐");
        let oversized = stream::iter([Ok(Bytes::from(vec![b'x'; MAX_ERROR_BYTES + 1]))]);
        assert_eq!(
            read_rejection(response(
                StatusCode::PAYMENT_REQUIRED,
                "application/json",
                None,
                Box::pin(oversized)
            ))
            .await
            .body,
            fallback_body()
        );
        let fragments = stream::repeat(Ok(Bytes::new()));
        let started = tokio::time::Instant::now();
        assert_eq!(
            read_rejection(response(
                StatusCode::PAYMENT_REQUIRED,
                "application/json",
                None,
                Box::pin(fragments)
            ))
            .await
            .body,
            fallback_body()
        );
        assert_eq!(tokio::time::Instant::now(), started);
        let timed = response(
            StatusCode::PAYMENT_REQUIRED,
            "application/json",
            None,
            Box::pin(stream::pending()),
        )
        .with_body_timeouts(started + Duration::from_millis(20), Duration::from_secs(5));
        let result = read_rejection(timed).await;
        assert_eq!(result.body, fallback_body());
        assert_eq!(result.terminal_cause, None);
        assert_eq!(
            tokio::time::Instant::now() - started,
            Duration::from_millis(20)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn bad_gateway_uses_the_existing_discard_only_diagnostic_path() {
        for content_type in ["application/json", "text/plain"] {
            for (error, expected) in [
                (
                    super::super::upstream_response::UPSTREAM_HTTP2_RESET,
                    RequestTerminalCause::Http2Reset,
                ),
                (
                    super::super::upstream_response::UPSTREAM_HTTP2_GOAWAY,
                    RequestTerminalCause::Http2GoAway,
                ),
                (
                    super::super::upstream_response::UPSTREAM_READ_TIMEOUT,
                    RequestTerminalCause::ReadTimeout,
                ),
                (
                    super::super::upstream_response::UPSTREAM_REQUEST_TIMEOUT,
                    RequestTerminalCause::RequestTimeout,
                ),
                (
                    super::super::upstream_response::UPSTREAM_STREAM_ERROR,
                    RequestTerminalCause::StreamReadError,
                ),
            ] {
                let stream =
                    stream::iter([Ok(Bytes::from_static(b"private-body-canary")), Err(error)]);
                let result = read_rejection(response(
                    StatusCode::BAD_GATEWAY,
                    content_type,
                    None,
                    Box::pin(stream),
                ))
                .await;
                assert_eq!(result.body, fallback_body());
                assert_eq!(result.terminal_cause, Some(expected));
            }
        }
        let started = tokio::time::Instant::now();
        let result = read_rejection(response(
            StatusCode::BAD_GATEWAY,
            "application/json",
            None,
            Box::pin(stream::empty()),
        ))
        .await;
        assert_eq!(result.body, fallback_body());
        assert_eq!(result.terminal_cause, None);
        assert_eq!(tokio::time::Instant::now(), started);
        let result = read_rejection(response(
            StatusCode::BAD_GATEWAY,
            "application/json",
            None,
            Box::pin(stream::pending()),
        ))
        .await;
        assert_eq!(result.body, fallback_body());
        assert_eq!(result.terminal_cause, None);
        assert_eq!(tokio::time::Instant::now() - started, MAX_ERROR_WAIT);
        let capped = stream::iter([
            Ok(Bytes::from(vec![b'x'; 64 * 1024])),
            Err(super::super::upstream_response::UPSTREAM_HTTP2_RESET),
        ]);
        let result = read_rejection(response(
            StatusCode::BAD_GATEWAY,
            "application/json",
            None,
            Box::pin(capped),
        ))
        .await;
        assert_eq!(result.body, fallback_body());
        assert_eq!(result.terminal_cause, None);
        let started = tokio::time::Instant::now();
        let timed = response(
            StatusCode::BAD_GATEWAY,
            "application/json",
            None,
            Box::pin(stream::pending()),
        )
        .with_body_timeouts(started + Duration::from_millis(20), Duration::from_secs(5));
        assert_eq!(
            read_rejection(timed).await.terminal_cause,
            Some(RequestTerminalCause::RequestTimeout)
        );
        assert_eq!(
            tokio::time::Instant::now() - started,
            Duration::from_millis(20)
        );
    }
}
