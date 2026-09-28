use super::*;
use std::sync::{Mutex, OnceLock};

const DIAGNOSTIC_QUEUE_CAPACITY: usize = 64;

#[derive(Debug)]
struct Diagnostic {
    request_id: Uuid,
    content_type: &'static str,
    dispatch: tracing::Dispatch,
}

static DIAGNOSTIC_QUEUE: OnceLock<Mutex<Option<tokio::sync::mpsc::Sender<Diagnostic>>>> =
    OnceLock::new();

/// Record only response metadata. The current response abstraction cannot
/// safely tee an untrusted body without retaining it in the request path, so
/// non-JSON diagnostics intentionally do not preserve or inspect body bytes.
/// This path is bounded to one fixed-label event and never feeds retry or
/// classification decisions.
pub(super) fn observe(response: &UpstreamResponse, request_id: Uuid) {
    let content_type = content_type_class(response.headers());
    let queue = DIAGNOSTIC_QUEUE.get_or_init(|| Mutex::new(None));
    let diagnostic = Diagnostic {
        request_id,
        content_type,
        dispatch: tracing::dispatcher::get_default(Clone::clone),
    };
    let enqueue_result = {
        let mut sender = queue.lock().expect("diagnostic queue lock poisoned");
        if sender.as_ref().is_none_or(tokio::sync::mpsc::Sender::is_closed) {
            let (new_sender, mut receiver) =
                tokio::sync::mpsc::channel::<Diagnostic>(DIAGNOSTIC_QUEUE_CAPACITY);
            tokio::spawn(async move {
                while let Some(diagnostic) = receiver.recv().await {
                    tracing::dispatcher::with_default(&diagnostic.dispatch, || {
                        tracing::warn!(
                            request_id = %diagnostic.request_id,
                            stage = "codex_upstream_bad_request",
                            upstream_error_classification = "unclassifiable",
                            upstream_error_reason = "content_type",
                            upstream_content_type_class = diagnostic.content_type,
                            upstream_diagnostic_read = "not_attempted",
                            "Codex upstream rejected the request"
                        );
                    });
                }
            });
            *sender = Some(new_sender);
        }
        sender.as_ref().expect("diagnostic sender initialized").try_send(diagnostic)
    };
    if enqueue_result.is_err() {
        let queue_closed = {
            let sender = queue.lock().expect("diagnostic queue lock poisoned");
            sender.as_ref().is_none_or(tokio::sync::mpsc::Sender::is_closed)
        };
        if queue_closed {
            observe(response, request_id);
            return;
        }
    }
    if enqueue_result.is_err() {
        tracing::warn!(
            %request_id,
            stage = "codex_upstream_bad_request",
            upstream_error_classification = "unclassifiable",
            upstream_error_reason = "content_type",
            upstream_content_type_class = content_type,
            upstream_diagnostic_read = "not_attempted",
            upstream_diagnostic_enqueue = "dropped_queue_full",
            "Codex upstream diagnostic was dropped after bounded queue admission"
        );
    }
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
