use futures_util::StreamExt;
use http::StatusCode;
use serde_json::Value;
use std::time::Duration;

const MAX_REJECTION_BODY: usize = 4096;
const REJECTION_BODY_TIMEOUT: Duration = Duration::from_millis(500);

/// Inspect only a small, bounded error envelope. Provider text is never
/// returned or logged, and unknown rejections retain the existing fallback.
pub(super) async fn classify_auth_rejection(response: reqwest::Response) -> &'static str {
    if response.status() != StatusCode::FORBIDDEN
        || response
            .content_length()
            .is_some_and(|length| length > MAX_REJECTION_BODY as u64)
    {
        return "authentication_failed";
    }
    let permission_denied = tokio::time::timeout(REJECTION_BODY_TIMEOUT, async {
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else { return false };
            if body.len().saturating_add(chunk.len()) > MAX_REJECTION_BODY {
                return false;
            }
            body.extend_from_slice(&chunk);
        }
        let Ok(value) = serde_json::from_slice::<Value>(&body) else {
            return false;
        };
        let Some(error) = value.get("error") else {
            return false;
        };
        error.get("type").and_then(Value::as_str) == Some("model_permission")
            && error.get("code").and_then(Value::as_str) == Some("model_not_activated")
    })
    .await
    .unwrap_or(false);
    if permission_denied {
        "permission_denied"
    } else {
        "authentication_failed"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

    #[tokio::test]
    async fn recognized_model_permission_rejection_differs_from_invalid_key() {
        let server = MockServer::start().await;
        let permission = json!({"error": {
            "type": "model_permission", "code": "model_not_activated",
            "message": "provider text must never be returned"
        }});
        for (status, body, expected) in [
            (403, permission.clone(), "permission_denied"),
            (401, permission, "authentication_failed"),
            (
                401,
                json!({"error": {"type": "authentication_error", "code": "api_key_not_found"}}),
                "authentication_failed",
            ),
            (
                403,
                json!({"error": {"type": "model_permission", "code": "unknown"}}),
                "authentication_failed",
            ),
            (
                403,
                json!({"error": {"message": "model_permission model_not_activated"}}),
                "authentication_failed",
            ),
        ] {
            server.reset().await;
            Mock::given(path("/models"))
                .respond_with(ResponseTemplate::new(status).set_body_json(body))
                .mount(&server)
                .await;
            let response = reqwest::Client::new()
                .get(format!("{}/models", server.uri()))
                .send()
                .await
                .unwrap();
            assert_eq!(classify_auth_rejection(response).await, expected);
        }
    }

    #[tokio::test]
    async fn malformed_or_oversized_envelopes_keep_the_status_fallback() {
        let server = MockServer::start().await;
        for body in [
            "not JSON".to_owned(),
            json!({"error": {"type": "model_permission", "code": "model_not_activated"},
                "padding": "x".repeat(MAX_REJECTION_BODY)})
            .to_string(),
        ] {
            server.reset().await;
            Mock::given(path("/models"))
                .respond_with(ResponseTemplate::new(403).set_body_string(body))
                .mount(&server)
                .await;
            let response = reqwest::Client::new()
                .get(format!("{}/models", server.uri()))
                .send()
                .await
                .unwrap();
            assert_eq!(
                classify_auth_rejection(response).await,
                "authentication_failed"
            );
        }
    }
}
