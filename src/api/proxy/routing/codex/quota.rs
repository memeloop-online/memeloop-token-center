//! Classify structured supplier quota rejections while preserving response ownership.
use super::super::super::*;
use crate::db::{SupplierQuotaEvidence, UpstreamFailureKind};
use futures_util::{StreamExt, stream};

const MAX_BODY: usize = 64 * 1024;
const MAX_CHUNKS: usize = 256;
fn bounded_future(deadline: i64, now: i64) -> Option<i64> {
    (deadline > now).then_some(deadline)
}

fn retry_after(headers: &HeaderMap, now: i64) -> Option<i64> {
    let mut values = headers.get_all(header::RETRY_AFTER).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let value = value.to_str().ok()?.trim();
    if value.is_empty() || value.len() > 128 {
        return None;
    }
    if value.bytes().all(|byte| byte.is_ascii_digit()) {
        let seconds: i64 = value.parse().ok()?;
        return bounded_future(now.checked_add(seconds.checked_mul(1_000)?)?, now);
    }
    let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    bounded_future(date.timestamp_millis(), now)
}

fn classify(headers: &HeaderMap, body: Option<&[u8]>, now: i64) -> UpstreamFailureKind {
    let evidence = quota_evidence(headers, body, now);
    match (evidence.exhausted, evidence.reset_at) {
        (false, None) => UpstreamFailureKind::RateLimited,
        (exhausted, deadline) => UpstreamFailureKind::RateLimitedUntil {
            exhausted,
            // Diagnostic compatibility only. Durable evidence retains None.
            until: deadline.unwrap_or(0),
        },
    }
}

fn exhausted_error(error: &Value) -> bool {
    use crate::supplier_error::UpstreamReason;
    let message = match (error.get("message"), error.get("msg")) {
        (Some(_), Some(_)) => return false,
        (Some(message), None) | (None, Some(message)) => message.as_str(),
        _ => None,
    };
    let code = error.get("code").and_then(Value::as_str);
    let kind = error.get("type").and_then(Value::as_str);
    let reason = UpstreamReason::from_supplier(code.or(kind), message);
    if kind == Some("usage_limit_reached") {
        return [
            UpstreamReason::from_supplier(code, None),
            UpstreamReason::from_supplier(None, message),
        ]
        .into_iter()
        .all(|reason| reason.is_none_or(|reason| reason == UpstreamReason::InsufficientQuota));
    }
    matches!(
        code.or(kind),
        Some("insufficient_quota" | "insufficient_user_quota")
    ) && reason == Some(UpstreamReason::InsufficientQuota)
}

pub(in crate::api::proxy) fn quota_evidence(
    headers: &HeaderMap,
    body: Option<&[u8]>,
    now: i64,
) -> SupplierQuotaEvidence {
    let header_deadline = retry_after(headers, now);
    let value = body.and_then(|body| crate::api::sse::parse_unique_json(body).ok());
    let error = value
        .as_ref()
        .and_then(|value| match value.get("error") {
            Some(error) if error.is_object() => Some(error),
            None if value.is_object() => Some(value),
            _ => None,
        })
        .filter(|error| exhausted_error(error));
    let reset = error.and_then(|error| {
        error
            .get("resets_at")
            .and_then(Value::as_i64)
            .and_then(|seconds| seconds.checked_mul(1_000))
            .and_then(|deadline| bounded_future(deadline, now))
            .or_else(|| {
                error
                    .get("resets_in_seconds")
                    .and_then(Value::as_i64)
                    .filter(|seconds| *seconds > 0)
                    .and_then(|seconds| {
                        bounded_future(now.checked_add(seconds.checked_mul(1_000)?)?, now)
                    })
            })
    });
    let deadline = header_deadline.into_iter().chain(reset).max();
    SupplierQuotaEvidence {
        exhausted: error.is_some(),
        reset_at: deadline,
        reset_source: match (header_deadline, reset) {
            (Some(header), Some(reset)) if reset > header => "provider_reset",
            (Some(_), _) => "retry_after",
            (_, Some(_)) => "provider_reset",
            _ => "unknown",
        },
    }
}

pub(in crate::api::proxy) async fn classify_rate_limit(
    response: UpstreamResponse,
) -> (UpstreamResponse, UpstreamFailureKind) {
    let (response, kind, _) = classify_supplier_quota(response).await;
    (response, kind)
}

pub(in crate::api::proxy) async fn classify_stream_quota(
    response: UpstreamResponse,
    limits: crate::provider::SseFramingLimits,
    memory: &crate::gateway_body::memory::ProxyMemoryBudget,
) -> (
    UpstreamResponse,
    Option<(UpstreamFailureKind, SupplierQuotaEvidence)>,
) {
    let is_sse = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"));
    if !response.status().is_success()
        || !is_sse
        || response
            .headers()
            .get_all(header::CONTENT_ENCODING)
            .iter()
            .any(|encoding| !encoding.as_bytes().eq_ignore_ascii_case(b"identity"))
    {
        return (response, None);
    }
    let Ok(reservation) = memory.temporary(MAX_BODY * 8) else {
        return (response, None);
    };
    let mut parts = response.into_parts();
    let mut framer = crate::api::sse::BoundedSseFramer::with_limits(limits);
    let mut identity = crate::api::sse::ResponseIdentityGate::default();
    let mut prefix = Vec::new();
    let mut inspected = 0_usize;
    let rejection = 'read: loop {
        if prefix.len() >= MAX_CHUNKS {
            break None;
        }
        if !prefix.is_empty() && prefix.len().is_multiple_of(16) {
            tokio::task::yield_now().await;
        }
        let Some(chunk) = parts.stream.next().await else {
            break None;
        };
        let batch = match &chunk {
            Ok(bytes) if bytes.len() <= MAX_BODY.saturating_sub(inspected) => {
                inspected += bytes.len();
                Some(framer.push(bytes))
            }
            _ => None,
        };
        prefix.push(chunk);
        let Some(batch) = batch else {
            break None;
        };
        if batch.rejection.is_some() {
            break None;
        }
        if let Some(event) = batch.events.into_iter().next() {
            let Ok((name, data)) = crate::api::sse::parse_sse_event(&event) else {
                break 'read None;
            };
            let Some(data) = data else {
                break 'read None;
            };
            let Ok(value) = crate::api::sse::parse_unique_json(&data) else {
                break 'read None;
            };
            let payload_name = value.get("type").and_then(Value::as_str);
            if name
                .as_deref()
                .is_some_and(|name| payload_name.is_some_and(|payload| name != payload))
                || has_execution_evidence(&value)
                || value.get("response").is_some_and(has_execution_evidence)
            {
                break 'read None;
            }
            let event_name = payload_name.or(name.as_deref());
            if let Some(event_name) = event_name
                && identity.observe(event_name, &value).is_err()
            {
                break 'read None;
            }
            match event_name {
                Some("error" | "response.failed" | "response.error") | None => {
                    let error = if event_name.is_some_and(|name| name.starts_with("response.")) {
                        value.pointer("/response/error")
                    } else {
                        value
                            .get("error")
                            .or_else(|| (event_name == Some("error")).then_some(&value))
                    };
                    break 'read error
                        .filter(|error| error.is_object())
                        .and_then(|error| stream_quota_evidence(&parts.headers, error));
                }
                _ => break 'read None,
            }
        }
    };
    let response = UpstreamResponse::Prefetched {
        status: parts.status,
        headers: parts.headers,
        version: parts.version,
        content_length: parts.content_length,
        stream: Box::pin(
            stream::iter(prefix)
                .map(move |chunk| {
                    let _retained = &reservation;
                    chunk
                })
                .chain(parts.stream),
        ),
    };
    (response, rejection)
}

fn has_execution_evidence(value: &Value) -> bool {
    [
        "usage",
        "output",
        "choices",
        "content",
        "delta",
        "item",
        "tool_calls",
    ]
    .into_iter()
    .any(|field| {
        value
            .get(field)
            .is_some_and(|value| !value.is_null() && !value.as_array().is_some_and(Vec::is_empty))
    })
}

fn stream_quota_evidence(
    headers: &HeaderMap,
    error: &Value,
) -> Option<(UpstreamFailureKind, SupplierQuotaEvidence)> {
    use crate::supplier_error::UpstreamReason;
    let field = |name| match error.get(name) {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(value)) => Some(Some(value.as_str())),
        _ => None,
    };
    let code = field("code")?;
    let kind = field("type")?;
    let reason_for = |value| match value {
        "rate_limit_error" => Some(UpstreamReason::RateLimited),
        "usage_limit_reached" => Some(UpstreamReason::InsufficientQuota),
        "authentication_error" | "permission_error" => Some(UpstreamReason::AuthenticationInvalid),
        _ => UpstreamReason::from_supplier(Some(value), None),
    };
    let reason = reason_for(code.or(kind)?)?;
    if !matches!(
        reason,
        UpstreamReason::RateLimited | UpstreamReason::InsufficientQuota
    ) || [code, kind]
        .into_iter()
        .flatten()
        .filter_map(reason_for)
        .any(|other| other != reason)
        || [code, kind]
            .into_iter()
            .flatten()
            .any(|value| matches!(value, "overloaded_error" | "api_error"))
    {
        return None;
    }
    let message = match (error.get("message"), error.get("msg")) {
        (Some(_), Some(_)) => return None,
        (Some(message), None) | (None, Some(message)) => Some(message.as_str()?),
        _ => None,
    };
    if UpstreamReason::from_supplier(None, message).is_some_and(|other| other != reason) {
        return None;
    }
    let body = serde_json::to_vec(error).ok()?;
    let now = unix_millis();
    let mut evidence = quota_evidence(headers, Some(&body), now);
    evidence.exhausted = reason == UpstreamReason::InsufficientQuota;
    let failure = if evidence.exhausted || evidence.reset_at.is_some() {
        UpstreamFailureKind::RateLimitedUntil {
            exhausted: evidence.exhausted,
            until: evidence.reset_at.unwrap_or(0),
        }
    } else {
        UpstreamFailureKind::RateLimited
    };
    Some((failure, evidence))
}

pub(in crate::api::proxy) async fn classify_supplier_quota(
    response: UpstreamResponse,
) -> (UpstreamResponse, UpstreamFailureKind, SupplierQuotaEvidence) {
    if !matches!(
        response.status(),
        StatusCode::TOO_MANY_REQUESTS | StatusCode::FORBIDDEN
    ) {
        let evidence = quota_evidence(response.headers(), None, unix_millis());
        return (response, UpstreamFailureKind::RateLimited, evidence);
    }
    let mut parts = response.into_parts();
    let forbidden = parts.status == StatusCode::FORBIDDEN;
    if forbidden
        && (upstream_error::rejection_content_class(&parts.headers) != "json"
            || parts
                .headers
                .get_all(header::CONTENT_ENCODING)
                .iter()
                .any(|encoding| !encoding.as_bytes().eq_ignore_ascii_case(b"identity")))
    {
        return (
            UpstreamResponse::Prefetched {
                status: parts.status,
                headers: parts.headers,
                version: parts.version,
                content_length: parts.content_length,
                stream: parts.stream,
            },
            UpstreamFailureKind::Authentication,
            SupplierQuotaEvidence {
                exhausted: false,
                reset_at: None,
                reset_source: "unknown",
            },
        );
    }
    let mut prefix = Vec::new();
    let mut body = Vec::new();
    let max_body = if forbidden {
        upstream_error::MAX_ERROR_BYTES
    } else {
        MAX_BODY
    };
    let complete = loop {
        if prefix.len() >= MAX_CHUNKS {
            break false;
        }
        if !prefix.is_empty() && prefix.len().is_multiple_of(16) {
            tokio::task::yield_now().await;
        }
        match parts.stream.next().await {
            Some(Ok(chunk)) => {
                let fits = body.len().saturating_add(chunk.len()) <= max_body;
                if fits {
                    body.extend_from_slice(&chunk);
                }
                prefix.push(Ok(chunk));
                if !fits {
                    break false;
                }
            }
            Some(Err(error)) => {
                prefix.push(Err(error));
                break false;
            }
            None => break true,
        }
    };
    let now = unix_millis();
    let evidence = quota_evidence(&parts.headers, complete.then_some(body.as_slice()), now);
    let kind = if forbidden && !evidence.exhausted {
        UpstreamFailureKind::Authentication
    } else {
        classify(&parts.headers, complete.then_some(body.as_slice()), now)
    };
    let response = UpstreamResponse::Prefetched {
        status: parts.status,
        headers: parts.headers,
        version: parts.version,
        content_length: parts.content_length,
        stream: Box::pin(stream::iter(prefix).chain(parts.stream)),
    };
    (response, kind, evidence)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000_000;

    #[tokio::test]
    async fn stream_quota_requires_complete_preoutput_evidence_and_preserves_wire() {
        for (body, rejected) in [
            (
                "data: {\"error\":{\"code\":\"insufficient_quota\"}}\n\n",
                true,
            ),
            (
                "event: error\ndata: {\"type\":\"error\",\"code\":\"rate_limit_exceeded\"}\n\n",
                true,
            ),
            (
                "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"usage_limit_reached\"}}}\n\n",
                true,
            ),
            (
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-a\",\"output\":[],\"usage\":null}}\n\ndata: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp-a\",\"error\":{\"code\":\"insufficient_quota\"}}}\n\n",
                false,
            ),
            (
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"retry after plan window\"}}\n\n",
                true,
            ),
            (
                "data: {\"error\":{\"code\":null,\"type\":\"usage_limit_reached\"}}\n\n",
                true,
            ),
            (
                "data: {\"error\":{\"code\":\"insufficient_quota\",\"type\":\"rate_limit_error\"}}\n\n",
                false,
            ),
            (
                "data: {\"error\":{\"code\":\"invalid_api_key\",\"type\":\"usage_limit_reached\"}}\n\n",
                false,
            ),
            (
                "data: {\"error\":{\"code\":123,\"type\":\"usage_limit_reached\"}}\n\n",
                false,
            ),
            (
                "data: {\"error\":{\"type\":\"overloaded_error\"}}\n\n",
                false,
            ),
            (
                ": heartbeat\n\ndata: {\"error\":{\"type\":\"rate_limit_error\"}}\n\n",
                false,
            ),
            (
                "data: {\"error\":{\"code\":\"invalid_api_key\"}}\n\n",
                false,
            ),
            (
                "data: {\"error\":{\"message\":\"Insufficient quota\"}}\n\n",
                false,
            ),
            (
                "data: {\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"Invalid API key\"}}\n\n",
                false,
            ),
            (
                "data: {\"error\":{\"code\":\"insufficient_quota\",\"code\":\"invalid_api_key\"}}\n\n",
                false,
            ),
            (
                "event: response.completed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"insufficient_quota\"}}}\n\n",
                false,
            ),
            (
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call-a\"}]}}]}\n\ndata: {\"error\":{\"code\":\"insufficient_quota\"}}\n\n",
                false,
            ),
            (
                "data: {\"type\":\"response.failed\",\"response\":{\"usage\":{\"input_tokens\":4},\"error\":{\"code\":\"insufficient_quota\"}}}\n\n",
                false,
            ),
            (
                "data: {\"type\":\"response.failed\",\"response\":{\"output\":[{\"type\":\"function_call\"}],\"error\":{\"code\":\"insufficient_quota\"}}}\n\n",
                false,
            ),
            (
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-a\"}}\n\ndata: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp-b\",\"error\":{\"code\":\"insufficient_quota\"}}}\n\n",
                false,
            ),
            (
                "data: {\"error\":{\"code\":\"insufficient_quota\"}}\n",
                false,
            ),
        ] {
            for split in [1, body.len() / 2, body.len()] {
                let chunks = body
                    .as_bytes()
                    .chunks(split)
                    .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                    .collect::<Vec<_>>();
                let mut headers = HeaderMap::new();
                headers.insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/event-stream"),
                );
                let memory = crate::gateway_body::memory::ProxyMemoryBudget::new(4 * 1024 * 1024);
                let response = UpstreamResponse::Prefetched {
                    status: StatusCode::OK,
                    headers,
                    version: http::Version::HTTP_2,
                    content_length: None,
                    stream: Box::pin(stream::iter(chunks.clone())),
                };
                let (response, evidence) = classify_stream_quota(
                    response,
                    crate::provider::SseFramingLimits::default(),
                    &memory,
                )
                .await;
                assert_eq!(evidence.is_some(), rejected, "{body} split={split}");
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.bytes_stream().collect::<Vec<_>>().await, chunks);
                assert_eq!(memory.snapshot().0, 0);
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stream_quota_uses_transport_timeout_and_cancellation_without_private_deadline() {
        for interrupted in [false, true] {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            );
            let memory = crate::gateway_body::memory::ProxyMemoryBudget::new(4 * 1024 * 1024);
            let body =
                Bytes::from_static(b"data: {\"error\":{\"code\":\"insufficient_quota\"}}\n\n");
            let response = UpstreamResponse::Prefetched {
                status: StatusCode::OK,
                headers,
                version: http::Version::HTTP_2,
                content_length: None,
                stream: Box::pin(stream::once(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    if interrupted {
                        Err(upstream_response::UPSTREAM_READ_TIMEOUT)
                    } else {
                        Ok(body)
                    }
                })),
            };
            let (response, evidence) = classify_stream_quota(
                response,
                crate::provider::SseFramingLimits::default(),
                &memory,
            )
            .await;
            assert_eq!(evidence.is_some(), !interrupted);
            let chunks = response.bytes_stream().collect::<Vec<_>>().await;
            if interrupted {
                assert_eq!(chunks, vec![Err(upstream_response::UPSTREAM_READ_TIMEOUT)]);
            }
            assert_eq!(memory.snapshot().0, 0);
        }
        let memory = crate::gateway_body::memory::ProxyMemoryBudget::new(4 * 1024 * 1024);
        let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        let response = UpstreamResponse::Prefetched {
            status: StatusCode::OK,
            headers,
            version: http::Version::HTTP_2,
            content_length: None,
            stream: Box::pin(stream::once(async move {
                let _sender = sender;
                std::future::pending::<Result<Bytes, &'static str>>().await
            })),
        };
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                classify_stream_quota(
                    response,
                    crate::provider::SseFramingLimits::default(),
                    &memory
                ),
            )
            .await
            .is_err()
        );
        assert!(receiver.await.is_err());
        assert_eq!(memory.snapshot().0, 0);
    }

    #[test]
    fn structured_quota_codes_reuse_safe_supplier_classification_not_message_substrings() {
        for body in [
            r#"{"error":{"code":"insufficient_user_quota","type":"new_api_error","message":"用户额度不足, private-canary"}}"#,
            r#"{"error":{"code":"insufficient_quota"}}"#,
            r#"{"error":{"type":"insufficient_quota"}}"#,
        ] {
            assert!(quota_evidence(&HeaderMap::new(), Some(body.as_bytes()), NOW).exhausted);
        }
        for body in [
            r#"{"error":{"code":"invalid_api_key","message":"Invalid API key"}}"#,
            r#"{"error":{"code":"insufficient_user_quota","message":"Invalid API key"}}"#,
            r#"{"error":{"type":"usage_limit_reached","code":"invalid_api_key","message":"Insufficient quota"}}"#,
            r#"{"error":{"message":"The upstream account has insufficient quota"}}"#,
            r#"{"error":{"code":"insufficient_quota","code":"invalid_api_key"}}"#,
            r#"{"error":"permission denied","code":"insufficient_quota"}"#,
            r#"{"error":{"code":"insufficient_quota","message":"Insufficient quota","msg":"Invalid API key"}}"#,
        ] {
            assert!(
                !quota_evidence(&HeaderMap::new(), Some(body.as_bytes()), NOW).exhausted,
                "{body}"
            );
        }
    }

    #[tokio::test]
    async fn forbidden_capture_requires_complete_json_and_preserves_every_original_chunk() {
        for (body, content_type, exhausted) in [
            (
                r#"{"error":{"code":"insufficient_user_quota"}}"#,
                "application/json",
                true,
            ),
            (
                r#"{"error":{"code":"invalid_api_key"}}"#,
                "application/json",
                false,
            ),
            (
                r#"{"error":{"code":"insufficient_user_quota"}}"#,
                "text/html",
                false,
            ),
            (
                r#"{"error":{"code":"insufficient_user_quota"}}"#,
                "text/event-stream",
                false,
            ),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
            let chunks = vec![Ok(Bytes::from(body.to_owned()))];
            let response = UpstreamResponse::Prefetched {
                status: StatusCode::FORBIDDEN,
                headers,
                version: http::Version::HTTP_2,
                content_length: None,
                stream: Box::pin(stream::iter(chunks.clone())),
            };
            let (response, kind, evidence) = classify_supplier_quota(response).await;
            assert_eq!(evidence.exhausted, exhausted);
            assert_eq!(
                kind.as_str(),
                if exhausted {
                    "quota_exhausted"
                } else {
                    "authentication"
                }
            );
            assert_eq!(response.bytes_stream().collect::<Vec<_>>().await, chunks);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn interrupted_rejection_preserves_shared_transport_deadline_and_read_errors() {
        use crate::api::proxy::upstream_response::{
            UPSTREAM_READ_TIMEOUT, UPSTREAM_REQUEST_TIMEOUT,
        };
        for status in [StatusCode::FORBIDDEN, StatusCode::TOO_MANY_REQUESTS] {
            for (request_seconds, read_seconds, error) in [
                (5, 1, UPSTREAM_READ_TIMEOUT),
                (1, 5, UPSTREAM_REQUEST_TIMEOUT),
            ] {
                let prefix = Bytes::from_static(br#"{"error":{"code":"insufficient_quota""#);
                let mut headers = HeaderMap::new();
                headers.insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                let response = UpstreamResponse::Prefetched {
                    status,
                    headers,
                    version: http::Version::HTTP_2,
                    content_length: None,
                    stream: Box::pin(stream::iter([Ok(prefix.clone())]).chain(stream::once(
                        async {
                            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                            Ok(Bytes::from_static(b"}}"))
                        },
                    ))),
                }
                .with_body_timeouts(
                    tokio::time::Instant::now() + std::time::Duration::from_secs(request_seconds),
                    std::time::Duration::from_secs(read_seconds),
                );
                let (response, kind, evidence) = classify_supplier_quota(response).await;
                assert!(!evidence.exhausted);
                assert_eq!(
                    kind,
                    if status == StatusCode::FORBIDDEN {
                        UpstreamFailureKind::Authentication
                    } else {
                        UpstreamFailureKind::RateLimited
                    }
                );
                assert_eq!(
                    response.bytes_stream().collect::<Vec<_>>().await,
                    vec![Ok(prefix), Err(error)]
                );
            }
        }
    }

    #[tokio::test]
    async fn cancellation_drops_the_owned_rejection_body() {
        let (polled, body_polled) = tokio::sync::oneshot::channel();
        let (owned, body_dropped) = tokio::sync::oneshot::channel::<()>();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let response = UpstreamResponse::Prefetched {
            status: StatusCode::FORBIDDEN,
            headers,
            version: http::Version::HTTP_2,
            content_length: None,
            stream: Box::pin(stream::once(async move {
                let _owned = owned;
                polled.send(()).unwrap();
                std::future::pending::<Result<Bytes, &'static str>>().await
            })),
        };
        let task = tokio::spawn(classify_supplier_quota(response));
        body_polled.await.unwrap();
        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        assert!(body_dropped.await.is_err());
    }

    #[test]
    fn unknown_malformed_and_duplicate_resets_remain_unknown() {
        let mut headers = HeaderMap::new();
        let exhausted = br#"{"error":{"type":"usage_limit_reached","resets_at":"tomorrow","resets_in_seconds":-3}}"#;
        for value in ["", "-1", "garbage", "9999999999999999999999999999"] {
            headers.insert(header::RETRY_AFTER, HeaderValue::from_str(value).unwrap());
            let evidence = quota_evidence(&headers, Some(exhausted), NOW);
            assert!(evidence.exhausted);
            assert_eq!(evidence.reset_at, None);
            assert_eq!(evidence.reset_source, "unknown");
        }
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("120"));
        let evidence = quota_evidence(&headers, Some(exhausted), NOW);
        assert_eq!(evidence.reset_at, Some(NOW + 120_000));
        assert_eq!(evidence.reset_source, "retry_after");
        headers.append(header::RETRY_AFTER, HeaderValue::from_static("60"));
        assert_eq!(
            quota_evidence(&headers, Some(exhausted), NOW).reset_at,
            None
        );
        let evidence = quota_evidence(
            &HeaderMap::new(),
            Some(br#"{"error":{"type":"usage_limit_reached","resets_in_seconds":42}}"#),
            NOW,
        );
        assert_eq!(evidence.reset_at, Some(NOW + 42_000));
        assert_eq!(evidence.reset_source, "provider_reset");
        assert!(
            !quota_evidence(
                &HeaderMap::new(),
                Some(br#"{"error":{"type":"rate_limit_error"}}"#),
                NOW
            )
            .exhausted
        );
    }

    #[test]
    fn exact_exhaustion_reset_and_header_are_bounded_and_distinct_from_throttling() {
        let headers = HeaderMap::new();
        for (body, seconds) in [
            (
                r#"{"error":{"type":"usage_limit_reached","resets_at":1700003600,"resets_in_seconds":1}}"#,
                3600,
            ),
            (
                r#"{"type":"usage_limit_reached","resets_in_seconds":123}"#,
                123,
            ),
        ] {
            assert_eq!(
                classify(&headers, Some(body.as_bytes()), NOW),
                UpstreamFailureKind::RateLimitedUntil {
                    exhausted: true,
                    until: NOW + seconds * 1000
                }
            );
        }
        for body in [
            r#"{"error":{"type":"rate_limit_error","resets_in_seconds":3600}}"#,
            r#"{"error":{"message":"usage_limit_reached"}}"#,
            r#"{"error":{"type":"usage_limit_reached","type":"rate_limit_error"}}"#,
            "not json",
        ] {
            assert_eq!(
                classify(&headers, Some(body.as_bytes()), NOW),
                UpstreamFailureKind::RateLimited
            );
        }
        let mut headers = HeaderMap::new();
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("999999999"));
        assert_eq!(
            classify(&headers, None, NOW),
            UpstreamFailureKind::RateLimitedUntil {
                exhausted: false,
                until: NOW + 999999999000
            }
        );
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("-1"));
        assert_eq!(
            classify(&headers, None, NOW),
            UpstreamFailureKind::RateLimited
        );
        headers.insert(
            header::RETRY_AFTER,
            HeaderValue::from_static("Tue, 14 Nov 2023 23:13:20 GMT"),
        );
        assert_eq!(
            classify(&headers, None, NOW),
            UpstreamFailureKind::RateLimitedUntil {
                exhausted: false,
                until: NOW + 3_600_000
            }
        );
        headers.append(header::RETRY_AFTER, HeaderValue::from_static("1"));
        assert_eq!(
            classify(&headers, None, NOW),
            UpstreamFailureKind::RateLimited
        );
    }

    #[tokio::test]
    async fn bounded_capture_preserves_original_chunks_and_transport_failure() {
        for chunks in [
            vec![Ok(Bytes::from_static(
                b"{\"error\":{\"type\":\"usage_limit_reached\"}}",
            ))],
            vec![
                Ok(Bytes::from(vec![b'x'; MAX_BODY + 1])),
                Ok(Bytes::from_static(b"tail")),
            ],
            vec![Ok(Bytes::from_static(b"partial")), Err("upstream_stream")],
        ] {
            let response = UpstreamResponse::Prefetched {
                status: StatusCode::TOO_MANY_REQUESTS,
                headers: HeaderMap::new(),
                version: http::Version::HTTP_2,
                content_length: None,
                stream: Box::pin(stream::iter(chunks.clone())),
            };
            let (response, _) = classify_rate_limit(response).await;
            assert_eq!(response.bytes_stream().collect::<Vec<_>>().await, chunks);
        }
    }

    #[tokio::test]
    async fn infinitely_ready_empty_chunks_stop_at_chunk_budget_and_preserve_suffix() {
        let polled = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = polled.clone();
        let response = UpstreamResponse::Prefetched {
            status: StatusCode::TOO_MANY_REQUESTS,
            headers: HeaderMap::new(),
            version: http::Version::HTTP_2,
            content_length: None,
            stream: Box::pin(stream::repeat_with(move || {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Bytes::new())
            })),
        };
        let (response, kind) = classify_rate_limit(response).await;
        let captured = polled.load(std::sync::atomic::Ordering::SeqCst);
        assert!(captured <= MAX_CHUNKS);
        assert_eq!(kind, UpstreamFailureKind::RateLimited);
        let chunks = response
            .bytes_stream()
            .take(captured + 2)
            .collect::<Vec<_>>()
            .await;
        assert_eq!(chunks, vec![Ok(Bytes::new()); captured + 2]);
        assert_eq!(
            polled.load(std::sync::atomic::Ordering::SeqCst),
            captured + 2
        );
    }

    #[tokio::test(start_paused = true)]
    async fn delayed429_body_uses_transport_policy_and_preserves_original_chunks() {
        let prefix = Bytes::from_static(b"{\"error\":");
        let suffix = Bytes::from_static(b"{\"type\":\"usage_limit_reached\"}}");
        let expected = vec![Ok(prefix.clone()), Ok(suffix.clone())];
        let delayed = stream::once(async move {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            Ok(suffix)
        });
        let response = UpstreamResponse::Prefetched {
            status: StatusCode::TOO_MANY_REQUESTS,
            headers: HeaderMap::new(),
            version: http::Version::HTTP_2,
            content_length: None,
            stream: Box::pin(stream::once(async move { Ok(prefix) }).chain(delayed)),
        };
        let (response, kind) = classify_rate_limit(response).await;
        assert!(matches!(
            kind,
            UpstreamFailureKind::RateLimitedUntil {
                exhausted: true,
                ..
            }
        ));
        assert_eq!(response.bytes_stream().collect::<Vec<_>>().await, expected);
    }
}
