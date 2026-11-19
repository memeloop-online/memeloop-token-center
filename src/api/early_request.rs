use std::sync::{Arc, Mutex};

use axum::{
    http::{HeaderValue, StatusCode},
    response::Response,
};
use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::{AppState, db::EarlyRequestTerminal, error::PublicErrorCode, model::AuthenticatedKey};

tokio::task_local! {
    static METADATA: Arc<Mutex<Metadata>>;
}

#[derive(Default)]
struct Metadata {
    model: String,
    admitted: bool,
}

/// Owned media execution must publish its admission handoff to the caller's
/// observer, even though Tokio does not inherit task-local values.
pub(super) fn preserve_observation<F: std::future::Future>(
    future: F,
) -> impl std::future::Future<Output = F::Output> {
    let metadata = METADATA.try_with(Arc::clone).ok();
    async move {
        match metadata {
            Some(metadata) => METADATA.scope(metadata, future).await,
            None => future.await,
        }
    }
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
        let _ = METADATA.try_with(|current| {
            current
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .model = safe_model(model).to_owned()
        });
    }
}

pub(super) fn mark_admitted() {
    let _ = METADATA.try_with(|current| {
        current
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .admitted = true
    });
}

pub(super) fn is_pre_admission_failure(status: StatusCode) -> bool {
    (status.is_client_error() || status.is_server_error())
        && METADATA
            .try_with(|metadata| {
                !metadata
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .admitted
            })
            .unwrap_or(false)
}

pub(super) async fn observe(
    state: &AppState,
    key: &AuthenticatedKey,
    protocol: &'static str,
    operation: BoxFuture<'_, Response>,
) -> Response {
    let context = super::proxy_diagnostics::Context::current();
    let created_at = crate::db::unix_millis()
        .saturating_sub(context.elapsed_millis_at(std::time::Instant::now()));
    METADATA
        .scope(Arc::new(Mutex::new(Metadata::default())), async {
            let mut response = operation.await;
            if is_pre_admission_failure(response.status()) {
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
                    model: METADATA.with(|metadata| {
                        metadata
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .model
                            .clone()
                    }),
                    status_code: response.status().as_u16(),
                    error_code: error_code.to_owned(),
                };
                observe_failure(context.request_id, response.status().as_u16(), error_code);
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

fn observe_failure(request_id: uuid::Uuid, status: u16, error_code: &'static str) {
    // Emit independently of the journal and its database replay. The public
    // error code is observed evidence, not a SQL/transport root-cause label.
    // This boundary has no captured upstream/member or more specific phase;
    // leave them absent rather than inventing them from a 503 response.
    tracing::warn!(
        %request_id,
        phase = "pre_admission",
        terminal_cause_code = "gateway_pre_admission",
        status,
        error_code,
        "gateway pre-admission failure observed"
    );
}

#[cfg(test)]
mod diagnostics_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn owned_worker_admission_is_visible_only_to_its_request_observer() {
        METADATA
            .scope(Arc::new(Mutex::new(Metadata::default())), async {
                assert!(is_pre_admission_failure(StatusCode::BAD_REQUEST));
                let operation = preserve_observation(async {
                    tokio::task::yield_now().await;
                    mark_admitted();
                });
                tokio::spawn(operation).await.unwrap();
                assert!(!is_pre_admission_failure(StatusCode::BAD_GATEWAY));
                METADATA
                    .scope(Arc::new(Mutex::new(Metadata::default())), async {
                        assert!(is_pre_admission_failure(StatusCode::BAD_REQUEST));
                    })
                    .await;
            })
            .await;
    }

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
