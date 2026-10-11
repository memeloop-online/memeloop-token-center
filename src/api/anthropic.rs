//! Shared Anthropic Messages gateway compatibility helpers.
//!
//! Anthropic deliberately extends the Messages protocol through headers and
//! request fields.  Keep the forwarding surface here, rather than teaching
//! individual provider adapters a frozen list of currently observed beta
//! values.  Provider-specific dialect translation belongs at the adapter
//! boundary; this module only preserves the client-facing Messages contract.

use axum::http::{HeaderMap, HeaderValue, header};
use reqwest::RequestBuilder;

use crate::oauth::claude::OAUTH_BETA_HEADER;

pub(super) const DEFAULT_VERSION: &str = "2023-06-01";

/// Apply the open-ended Anthropic header surface to an outbound Messages
/// request. Credentials are intentionally excluded: MTC authenticates the
/// caller independently and applies the selected upstream credential later.
pub(super) fn apply_request_headers(
    mut request: RequestBuilder,
    inbound: &HeaderMap,
    requires_oauth_capability: bool,
) -> RequestBuilder {
    for (name, value) in inbound {
        let name = name.as_str();
        if name.starts_with("anthropic-telemetry-")
            || name.starts_with("anthropic-analytics-")
            || name.starts_with("x-claude-code-telemetry-")
            || name.starts_with("x-claude-code-analytics-")
        {
            continue;
        }
        let forward = (name.starts_with("anthropic-")
            && name != "anthropic-version"
            && name != "anthropic-beta")
            || name.starts_with("x-claude-code-");
        if forward {
            request = request.header(name, value);
        }
    }

    request = request.header(
        "anthropic-version",
        inbound
            .get("anthropic-version")
            .cloned()
            .unwrap_or_else(|| HeaderValue::from_static(DEFAULT_VERSION)),
    );

    if let Some(beta) = combined_beta(inbound, requires_oauth_capability) {
        request = request.header("anthropic-beta", beta);
    }
    request
}

pub(super) fn apply_declared_request_headers(
    request: RequestBuilder,
    inbound: &HeaderMap,
    fixed: &crate::provider::FixedProtocolHeaders,
    requires_oauth_capability: bool,
) -> Result<RequestBuilder, crate::error::AppError> {
    fixed.validate()?;
    let mut combined = inbound.clone();
    for capability in &fixed.anthropic_beta {
        combined.append(
            "anthropic-beta",
            HeaderValue::from_str(capability).map_err(|_| {
                crate::error::AppError::BadRequest("invalid fixed protocol headers".into())
            })?,
        );
    }
    if let Some(version) = &fixed.anthropic_version {
        combined.insert(
            "anthropic-version",
            HeaderValue::from_str(version).map_err(|_| {
                crate::error::AppError::BadRequest("invalid fixed protocol headers".into())
            })?,
        );
    }
    Ok(apply_request_headers(
        request,
        &combined,
        requires_oauth_capability,
    ))
}

/// Select the Messages response headers that Claude Code consumes for retry
/// decisions and plan-limit display.  They are copied to both buffered and
/// streamed downstream responses.
pub(super) fn downstream_response_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut selected = HeaderMap::new();
    for (name, value) in upstream {
        let name_text = name.as_str();
        if name == header::RETRY_AFTER
            || name_text == "retry-after-ms"
            || name_text == "x-should-retry"
            || name_text.starts_with("anthropic-ratelimit-")
        {
            selected.append(name.clone(), value.clone());
        }
    }
    selected
}

pub(super) fn append_response_headers(response: &mut HeaderMap, selected: &HeaderMap) {
    for (name, value) in selected {
        response.append(name.clone(), value.clone());
    }
}

fn combined_beta(inbound: &HeaderMap, requires_oauth_capability: bool) -> Option<HeaderValue> {
    let mut values = Vec::<String>::new();
    for value in inbound.get_all("anthropic-beta") {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for capability in value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if !values.iter().any(|existing| existing == capability) {
                values.push(capability.to_owned());
            }
        }
    }
    if requires_oauth_capability && !values.iter().any(|value| value == OAUTH_BETA_HEADER) {
        values.push(OAUTH_BETA_HEADER.to_owned());
    }
    (!values.is_empty())
        .then(|| HeaderValue::from_str(&values.join(",")).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_headers_merge_features_without_overriding_credentials() {
        let fixed = crate::provider::FixedProtocolHeaders {
            anthropic_version: Some("2023-06-01".into()),
            anthropic_beta: vec!["subscription-v1".into(), "subscription-v1".into()],
        };
        let mut inbound = HeaderMap::new();
        inbound.insert(
            "authorization",
            HeaderValue::from_static("Bearer downstream-secret"),
        );
        inbound.insert(
            "anthropic-version",
            HeaderValue::from_static("spoofed-version"),
        );
        inbound.insert("anthropic-beta", HeaderValue::from_static("tools-v1"));
        let request = apply_declared_request_headers(
            reqwest::Client::new()
                .post("https://provider.example/messages")
                .bearer_auth("host-credential"),
            &inbound,
            &fixed,
            false,
        )
        .unwrap()
        .build()
        .unwrap();
        assert_eq!(request.headers()["authorization"], "Bearer host-credential");
        assert_eq!(request.headers()["anthropic-version"], "2023-06-01");
        assert_eq!(
            request.headers()["anthropic-beta"],
            "tools-v1,subscription-v1"
        );
        assert_eq!(
            request.headers().get_all("anthropic-beta").iter().count(),
            1
        );
        let malicious = serde_json::json!({"authorization":"Bearer injected"});
        assert!(
            serde_json::from_value::<crate::provider::FixedProtocolHeaders>(malicious).is_err()
        );
        for value in [
            "feature\r\nauthorization: injected",
            "${access_token}",
            "feature,other",
        ] {
            let invalid = crate::provider::FixedProtocolHeaders {
                anthropic_version: None,
                anthropic_beta: vec![value.into()],
            };
            assert!(invalid.validate().is_err());
        }
    }

    #[test]
    fn selects_open_set_anthropic_and_claude_code_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        headers.append(
            "anthropic-beta",
            HeaderValue::from_static("prompt-caching-2024-07-31"),
        );
        headers.append(
            "anthropic-beta",
            HeaderValue::from_static("context-management-2025-06-27"),
        );
        headers.insert(
            "x-claude-code-agent-id",
            HeaderValue::from_static("agent-a"),
        );
        headers.insert("x-other", HeaderValue::from_static("do-not-forward"));
        headers.insert(
            "anthropic-telemetry-event",
            HeaderValue::from_static("optional-event"),
        );
        headers.insert(
            "x-claude-code-analytics-session",
            HeaderValue::from_static("optional-session"),
        );

        let beta = combined_beta(&headers, true).expect("merged beta");
        assert_eq!(
            beta,
            "prompt-caching-2024-07-31,context-management-2025-06-27,oauth-2025-04-20"
        );
        let selected = downstream_response_headers(&headers);
        assert!(selected.is_empty());

        let request = apply_request_headers(
            reqwest::Client::new().post("http://localhost/messages"),
            &headers,
            true,
        )
        .build()
        .unwrap();
        assert_eq!(request.headers()["anthropic-version"], "2023-06-01");
        assert_eq!(
            request.headers()["anthropic-beta"],
            "prompt-caching-2024-07-31,context-management-2025-06-27,oauth-2025-04-20"
        );
        assert_eq!(request.headers()["x-claude-code-agent-id"], "agent-a");
        assert!(request.headers().get("x-other").is_none());
        assert!(request.headers().get("anthropic-telemetry-event").is_none());
        assert!(
            request
                .headers()
                .get("x-claude-code-analytics-session")
                .is_none()
        );
    }

    #[test]
    fn preserves_retry_and_limit_headers_only() {
        let mut headers = HeaderMap::new();
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("2"));
        headers.insert("retry-after-ms", HeaderValue::from_static("2000"));
        headers.insert("x-should-retry", HeaderValue::from_static("true"));
        headers.insert(
            "anthropic-ratelimit-requests-remaining",
            HeaderValue::from_static("12"),
        );
        headers.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            HeaderValue::from_static("0.5"),
        );
        headers.insert("x-upstream-private", HeaderValue::from_static("omit"));

        let selected = downstream_response_headers(&headers);
        assert_eq!(selected[header::RETRY_AFTER], "2");
        assert_eq!(selected["retry-after-ms"], "2000");
        assert_eq!(selected["x-should-retry"], "true");
        assert_eq!(
            selected["anthropic-ratelimit-unified-5h-utilization"],
            "0.5"
        );
        assert_eq!(selected["anthropic-ratelimit-requests-remaining"], "12");
        assert!(selected.get("x-upstream-private").is_none());
    }
}
