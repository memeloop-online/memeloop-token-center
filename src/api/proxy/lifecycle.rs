use super::*;

mod cancellation;
pub(super) use cancellation::CancellationGuard;

/// Unlike health failover exhaustion, local dispatch overload keeps its typed
/// public body in both the response and the durable local-error archive.
pub(super) async fn finish_dispatch_failure(
    request: &BufferedRequest<'_>,
    error: crate::codex_clients::DispatchError,
    last_dispatched_upstream: Option<(Uuid, Uuid)>,
) -> Result<Response, AppError> {
    use crate::codex_clients::DispatchError;
    let (status, body, retryable) = match error {
        DispatchError::Capacity | DispatchError::Timeout => {
            (StatusCode::SERVICE_UNAVAILABLE, error.overload_body(), true)
        }
        DispatchError::InvalidPolicy => (
            StatusCode::BAD_GATEWAY,
            json!({"error": {"type": "upstream_error", "code": "invalid_transport_policy", "message": "invalid Codex transport policy"}}),
            false,
        ),
    };
    let mut response = finish_local_buffered_error_with_upstream_attribution(
        request,
        status,
        Bytes::from(serde_json::to_vec(&body).map_err(|_| AppError::internal("lifecycle_finish_dispatch_failure_1"))?),
        "application/json",
        (
            TokenUsage::default(),
            crate::model::RequestUsageBasis::NotObserved,
        ),
        Some(error.code().to_owned()),
        ProxyRequestUpstreamAttribution::LastDispatched(last_dispatched_upstream),
    )
    .await?;
    if retryable {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    Ok(response)
}
pub(super) async fn finish_unavailable(
    request: &BufferedRequest<'_>,
    error_code: &str,
    last_dispatched_upstream: Option<(Uuid, Uuid)>,
) -> Result<Response, AppError> {
    let mut response = finish_local_buffered_error_with_upstream_attribution(
        request,
        StatusCode::SERVICE_UNAVAILABLE,
        Bytes::from(crate::error::explained_failure_body(
            "当前没有健康的上游",
            "no healthy upstream is currently available",
            error_code,
            crate::error::ErrorFacts::new(),
        )),
        "application/json",
        (TokenUsage::default(), crate::model::RequestUsageBasis::NotObserved),
        Some(error_code.to_owned()),
        ProxyRequestUpstreamAttribution::LastDispatched(last_dispatched_upstream),
    )
    .await?;
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    Ok(response)
}

pub(super) async fn run_bounded_proxy_lifecycle<F>(
    deadline: tokio::time::Instant,
    lifecycle: F,
) -> Result<F::Output, tokio::time::error::Elapsed>
where
    F: std::future::Future,
{
    tokio::time::timeout_at(deadline, lifecycle).await
}
