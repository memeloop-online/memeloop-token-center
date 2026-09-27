use super::*;

const DIAGNOSTIC_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// No untrusted strings leave this diagnostic boundary. In particular, inferred
/// body semantics never enter the transport's retry classification.
pub(super) async fn observe(response: UpstreamResponse, request_id: Uuid) {
    let content_type = content_type_class(response.headers());
    let result = if response
        .content_length()
        .is_some_and(|length| length > MAX_RETRYABLE_ERROR_BYTES as u64)
    {
        Err("too_large")
    } else {
        match tokio::time::timeout(DIAGNOSTIC_WAIT, read_complete(response)).await {
            Ok(result) => result,
            Err(_) => Err("timed_out"),
        }
    };
    let (read_result, diagnostic) = match result {
        Ok(body) => ("complete", body_diagnostic(&body)),
        Err(reason) => (reason, unknown_diagnostic()),
    };
    tracing::warn!(
        %request_id,
        stage = "codex_upstream_bad_request",
        upstream_error_classification = "unclassifiable",
        upstream_error_reason = "content_type",
        upstream_content_type_class = content_type,
        upstream_diagnostic_read = read_result,
        upstream_body_reason = diagnostic.reason,
        upstream_error_type = diagnostic.error_type,
        upstream_error_code = diagnostic.error_code,
        upstream_error_param = diagnostic.error_param,
        "Codex upstream rejected the request"
    );
}

async fn read_complete(response: UpstreamResponse) -> Result<Vec<u8>, &'static str> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "read_failed")?;
        if chunk.len() > MAX_RETRYABLE_ERROR_BYTES.saturating_sub(body.len()) {
            return Err("too_large");
        }
        body.extend_from_slice(&chunk);
        // Even a continuously-ready stream of empty chunks must yield so the
        // outer diagnostic deadline can cancel the read.
        tokio::task::yield_now().await;
    }
    Ok(body)
}

fn content_type_class(headers: &http::HeaderMap) -> &'static str {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let Some(value) = values.next() else {
        return "missing";
    };
    if values.next().is_some() {
        return "multiple";
    }
    let Ok(value) = value.to_str() else {
        return "invalid";
    };
    match value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "text/plain" => "text_plain",
        "text/html" => "text_html",
        "application/octet-stream" => "octet_stream",
        _ => "other",
    }
}

fn unknown_diagnostic() -> BadRequestDiagnostic {
    BadRequestDiagnostic {
        error_type: None,
        error_code: None,
        error_param: None,
        reason: "unknown",
    }
}

fn body_diagnostic(body: &[u8]) -> BadRequestDiagnostic {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        return bad_request_diagnostic(&value);
    }
    let mut diagnostic = unknown_diagnostic();
    // Plaintext/HTML can suggest a fixed category, but never proves the
    // response came from the model backend rather than an edge proxy.
    if let Ok(text) = std::str::from_utf8(body) {
        diagnostic.reason = diagnostic_reason(Some(text), None);
    }
    diagnostic
}
