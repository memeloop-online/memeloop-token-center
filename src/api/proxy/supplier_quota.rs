use super::*;
use futures_util::StreamExt;

pub(super) fn publish_supplier_quota(
    state: &AppState,
    route: &ResolvedUpstream,
    request_id: Uuid,
    observed_at: i64,
    evidence: crate::db::SupplierQuotaEvidence,
) {
    IndependentProbeTarget::quota(evidence);
    let database = state.db.clone();
    let route = route.clone();
    state.routing_persistence.submit(4096, async move {
        database
            .store_supplier_quota(&route, request_id, observed_at, evidence)
            .await
    });
}

/// Observe the body only as the existing rejection reader consumes it. No
/// second POST, extra read, timeout or customer-path persistence await.
pub(super) fn observe_quota_body(
    response: UpstreamResponse,
    state: &AppState,
    route: &ResolvedUpstream,
    request_id: Uuid,
    observed_at: i64,
) -> UpstreamResponse {
    let parts = response.into_parts();
    let headers = parts.headers.clone();
    let stream = futures_util::stream::unfold(
        (
            parts.stream,
            Some(Vec::new()),
            state.clone(),
            route.clone(),
            headers,
        ),
        move |(mut source, mut body, state, route, headers)| async move {
            if let Some(chunk) = source.next().await {
                match (&mut body, &chunk) {
                    (Some(body), Ok(bytes))
                        if bytes.len()
                            <= upstream_error::MAX_ERROR_BYTES.saturating_sub(body.len()) =>
                    {
                        body.extend_from_slice(bytes);
                    }
                    _ => body = None,
                }
                return Some((chunk, (source, body, state, route, headers)));
            }
            if let Some(body) = body {
                let evidence = routing::quota_evidence(&headers, Some(&body), observed_at);
                publish_supplier_quota(&state, &route, request_id, observed_at, evidence);
            }
            None
        },
    );
    UpstreamResponse::Prefetched {
        status: parts.status,
        headers: parts.headers,
        version: parts.version,
        content_length: parts.content_length,
        stream: Box::pin(stream),
    }
}
