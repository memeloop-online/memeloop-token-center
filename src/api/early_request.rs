use std::cell::RefCell;

use axum::{http::HeaderValue, response::Response};
use serde_json::Value;

use crate::{AppState, db::EarlyRequestTerminal, error::PublicErrorCode, model::AuthenticatedKey};

tokio::task_local! {
    static METADATA: RefCell<Metadata>;
}

#[derive(Default)]
struct Metadata {
    model: String,
    admitted: bool,
}

pub(super) fn safe_model(model: &str) -> &str {
    if model.len() <= 200
        && model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:/@".contains(&byte))
    {
        model
    } else {
        ""
    }
}

pub(super) fn observe_model(model: Option<&Value>) {
    if let Some(model) = model.and_then(Value::as_str) {
        let _ =
            METADATA.try_with(|current| current.borrow_mut().model = safe_model(model).to_owned());
    }
}

pub(super) fn mark_admitted() {
    let _ = METADATA.try_with(|current| current.borrow_mut().admitted = true);
}

pub(super) async fn observe(
    state: &AppState,
    key: &AuthenticatedKey,
    protocol: &'static str,
    operation: impl std::future::Future<Output = Response>,
) -> Response {
    let context = super::proxy_diagnostics::Context::current();
    let created_at = crate::db::unix_millis()
        .saturating_sub(context.elapsed_millis_at(std::time::Instant::now()));
    METADATA
        .scope(RefCell::new(Metadata::default()), async {
            let mut response = operation.await;
            if (response.status().is_client_error() || response.status().is_server_error())
                && !METADATA.with(|metadata| metadata.borrow().admitted)
            {
                let error_code = response
                    .extensions()
                    .get::<PublicErrorCode>()
                    .map(|value| value.0)
                    .unwrap_or(match response.status().as_u16() {
                        400 | 422 => "invalid_request",
                        403 => "forbidden",
                        408 => "request_body_timeout",
                        413 => "request_body_too_large",
                        415 => "unsupported_media_type",
                        429 => "rate_limit_exceeded",
                        503 => "service_overloaded",
                        _ => "gateway_request_failed",
                    });
                let terminal = EarlyRequestTerminal {
                    request_id: context.request_id,
                    tenant_id: key.tenant_id,
                    key_id: key.key_id,
                    created_at,
                    completed_at: crate::db::unix_millis().max(created_at),
                    duration_ms: context.elapsed_millis_at(std::time::Instant::now()),
                    protocol: protocol.to_owned(),
                    model: METADATA.with(|metadata| metadata.borrow().model.clone()),
                    status_code: response.status().as_u16(),
                    error_code: error_code.to_owned(),
                };
                if !state.early_requests.append(terminal).await {
                    response.headers_mut().insert(
                        "x-mtc-request-recording",
                        HeaderValue::from_static("unavailable"),
                    );
                }
            }
            response
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::safe_model;

    #[test]
    fn model_metadata_is_bounded_and_excludes_payload_fragments() {
        assert_eq!(safe_model("gpt-6.1-sol"), "gpt-6.1-sol");
        assert_eq!(safe_model("provider/kimi-k3-256k"), "provider/kimi-k3-256k");
        assert_eq!(safe_model("Bearer secret"), "");
        assert_eq!(safe_model("model\nsecret"), "");
        assert_eq!(safe_model("{\"secret\":true}"), "");
        assert_eq!(safe_model(&"x".repeat(201)), "");
    }
}
