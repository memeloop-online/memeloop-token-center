use super::*;
use std::sync::OnceLock;

const DIAGNOSTIC_QUEUE_CAPACITY: usize = 64;

#[derive(Debug)]
struct Diagnostic {
    request_id: Uuid,
    content_type: &'static str,
}

static DIAGNOSTIC_QUEUE: OnceLock<tokio::sync::mpsc::Sender<Diagnostic>> = OnceLock::new();

/// Record only response metadata. The current response abstraction cannot
/// safely tee an untrusted body without retaining it in the request path, so
/// non-JSON diagnostics intentionally do not preserve or inspect body bytes.
/// This path is bounded to one fixed-label event and never feeds retry or
/// classification decisions.
pub(super) fn observe(response: &UpstreamResponse, request_id: Uuid) {
    let content_type = content_type_class(response.headers());
    let sender = DIAGNOSTIC_QUEUE.get_or_init(|| {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(DIAGNOSTIC_QUEUE_CAPACITY);
        tokio::spawn(async move {
            while let Some(diagnostic) = receiver.recv().await {
                tracing::warn!(
                    request_id = %diagnostic.request_id,
                    stage = "codex_upstream_bad_request",
                    upstream_error_classification = "unclassifiable",
                    upstream_error_reason = "content_type",
                    upstream_content_type_class = diagnostic.content_type,
                    upstream_diagnostic_read = "not_attempted",
                    "Codex upstream rejected the request"
                );
            }
        });
        sender
    });
    let _ = sender.try_send(Diagnostic {
        request_id,
        content_type,
    });
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
