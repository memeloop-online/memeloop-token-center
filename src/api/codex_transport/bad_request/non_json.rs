use super::*;

pub(super) fn observe(response: &UpstreamResponse, request_id: Uuid) {
    let content_type = content_type_class(response.headers());
    tracing::warn!(
        %request_id,
        stage = "codex_upstream_bad_request",
        upstream_error_classification = "unclassifiable",
        upstream_error_reason = "content_type",
        upstream_content_type_class = content_type,
        upstream_diagnostic_read = "not_attempted",
        "Codex upstream rejected the request"
    );
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
