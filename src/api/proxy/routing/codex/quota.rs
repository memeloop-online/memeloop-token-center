//! Read-only classification of definite HTTP 429 rejection, never a supplier
//! retry or quota reset. SSE failures do not establish that execution was free.
use super::super::super::*;
use crate::db::{SupplierQuotaEvidence, UpstreamFailureKind};
use futures_util::{StreamExt, stream};

const MAX_BODY: usize = 64 * 1024;
const MAX_CHUNKS: usize = 256;
fn bounded_future(deadline: i64, now: i64) -> Option<i64> {
    (deadline > now).then_some(deadline)
}

fn retry_after(headers: &HeaderMap, now: i64) -> Option<i64> {
    // Duplicate timing headers are ambiguous; never choose one arbitrarily.
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

pub(in crate::api::proxy) fn quota_evidence(
    headers: &HeaderMap,
    body: Option<&[u8]>,
    now: i64,
) -> SupplierQuotaEvidence {
    let header_deadline = retry_after(headers, now);
    let value = body.and_then(|body| crate::api::sse::parse_unique_json(body).ok());
    let error = value.as_ref().and_then(|value| {
        // Only the exact structured supplier signal qualifies as exhaustion.
        // Text matching and guessed HTTP200/5xx semantics are deliberately absent.
        [value.get("error"), Some(value)]
            .into_iter()
            .flatten()
            .find(|error| error.get("type").and_then(Value::as_str) == Some("usage_limit_reached"))
    });
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

pub(in crate::api::proxy) async fn classify_supplier_quota(
    response: UpstreamResponse,
) -> (UpstreamResponse, UpstreamFailureKind, SupplierQuotaEvidence) {
    if response.status() != StatusCode::TOO_MANY_REQUESTS {
        let evidence = quota_evidence(response.headers(), None, unix_millis());
        return (response, UpstreamFailureKind::RateLimited, evidence);
    }
    let mut parts = response.into_parts();
    let mut prefix = Vec::new();
    let mut body = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
    let read = async {
        loop {
            // Empty and permanently-ready chunks consume work and metadata
            // even when the byte budget is unchanged. Bound both, and yield
            // so a ready stream cannot starve the timeout/cancellation owner.
            if prefix.len() >= MAX_CHUNKS || tokio::time::Instant::now() >= deadline {
                return false;
            }
            if !prefix.is_empty() && prefix.len().is_multiple_of(16) {
                tokio::task::yield_now().await;
                if tokio::time::Instant::now() >= deadline {
                    return false;
                }
            }
            match parts.stream.next().await {
                Some(Ok(chunk)) => {
                    let fits = body.len().saturating_add(chunk.len()) <= MAX_BODY;
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
    };
    let complete = tokio::time::timeout_at(deadline, read)
        .await
        .unwrap_or(false);
    let now = unix_millis();
    let kind = classify(&parts.headers, complete.then_some(body.as_slice()), now);
    let evidence = quota_evidence(&parts.headers, complete.then_some(body.as_slice()), now);
    // Preserve the entire original stream including errors and unread suffix.
    // No secret error JSON enters logs, metrics, health rows or new archives.
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
    async fn timeout_restores_captured_prefix_and_delayed_unread_suffix() {
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
        assert_eq!(
            kind,
            UpstreamFailureKind::RateLimited,
            "partial JSON is not exhaustion evidence"
        );
        assert_eq!(response.bytes_stream().collect::<Vec<_>>().await, expected);
    }
}
