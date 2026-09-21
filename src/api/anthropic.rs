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
    strip_client_fingerprints: bool,
) -> RequestBuilder {
    // Wire-shimmed routes strip every client-supplied fingerprint header
    // (anthropic-* extras and x-claude-code-*): the upstream must see only
    // what the validated plugin set-headers and the core merge produce.
    // user-agent, x-app, x-client-request-id, x-stainless-* and x-api-key are
    // never forwarded by this transport in the first place.
    if !strip_client_fingerprints {
        for (name, value) in inbound {
            let name = name.as_str();
            let forward = (name.starts_with("anthropic-")
                && name != "anthropic-version"
                && name != "anthropic-beta")
                || name.starts_with("x-claude-code-");
            if forward {
                request = request.header(name, value);
            }
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
            false,
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
    }

    #[test]
    fn wire_shimmed_routes_strip_client_fingerprint_headers_but_keep_core_merge() {
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("prompt-caching-2024-07-31"),
        );
        headers.insert("anthropic-dangerous", HeaderValue::from_static("spoofed"));
        headers.insert(
            "x-claude-code-session-id",
            HeaderValue::from_static("client-claimed"),
        );
        headers.insert("x-anthropic-custom", HeaderValue::from_static("spoofed"));

        let request = apply_request_headers(
            reqwest::Client::new().post("http://localhost/messages"),
            &headers,
            true,
            true,
        )
        .build()
        .unwrap();
        assert_eq!(request.headers()["anthropic-version"], "2023-06-01");
        assert_eq!(
            request.headers()["anthropic-beta"],
            "prompt-caching-2024-07-31,oauth-2025-04-20"
        );
        assert!(request.headers().get("anthropic-dangerous").is_none());
        assert!(request.headers().get("x-claude-code-session-id").is_none());
        assert!(request.headers().get("x-anthropic-custom").is_none());
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
