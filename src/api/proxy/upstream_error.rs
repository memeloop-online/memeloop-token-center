use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use http::{HeaderMap, StatusCode, header};
use serde_json::{Value, json};

use super::upstream_response::UpstreamResponse;
use crate::{model::RequestTerminalCause, supplier_error::UpstreamReason};

pub(super) const MAX_ERROR_BYTES: usize = 16 * 1024;
const MAX_ERROR_WAIT: Duration = Duration::from_secs(5);

pub(in crate::api) struct Rejection {
    pub(in crate::api) body: Bytes,
    pub(super) terminal_cause: Option<RequestTerminalCause>,
}

pub(in crate::api) async fn read_reqwest_rejection(response: reqwest::Response) -> Rejection {
    read_rejection(UpstreamResponse::Reqwest(response)).await
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

pub(in crate::api) fn normalized_error_body(raw: &[u8]) -> Bytes {
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
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    let read = async {
        loop {
            match stream.next().await {
                Some(Ok(chunk)) if chunk.len() <= MAX_ERROR_BYTES.saturating_sub(body.len()) => {
                    body.extend_from_slice(&chunk);
                    if chunk.is_empty() {
                        tokio::task::yield_now().await;
                    }
                }
                Some(Ok(chunk)) => {
                    body.extend_from_slice(&chunk[..MAX_ERROR_BYTES.saturating_sub(body.len())]);
                    body.extend_from_slice(
                        b"\n[MTC: upstream error body truncated at 16384 bytes]",
                    );
                    return (normalized_error_body(&body), None);
                }
                Some(Err(error)) => {
                    return (
                        normalized_error_body(&body),
                        super::upstream_response::terminal_cause_from_stream_error(error),
                    );
                }
                None => return (normalized_error_body(&body), None),
            }
        }
    };
    let (body, terminal_cause) = tokio::time::timeout(MAX_ERROR_WAIT, read)
        .await
        .unwrap_or_else(|_| {
            body.extend_from_slice(
                b"\n[MTC: timed out reading upstream error body after 5 seconds]",
            );
            (normalized_error_body(&body), None)
        });
    Rejection {
        body,
        terminal_cause: terminal_cause.or(Some(RequestTerminalCause::UpstreamHttpStatus)),
    }
}

impl Rejection {
    pub(in crate::api) fn reason(&self) -> Option<UpstreamReason> {
        let value = crate::api::sse::parse_unique_json(&self.body).ok()?;
        crate::supplier_error::reason_from_value(&value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
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
