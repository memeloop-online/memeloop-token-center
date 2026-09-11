use super::*;
use crate::{db::UpstreamFailureKind, metrics::UpstreamHealthReason};

pub(in crate::api::proxy) fn status_failover_reason(
    status: StatusCode,
    route: &PreparedProxyRoute,
    candidate_available: bool,
    request_id: Uuid,
    candidate_rank: usize,
    outbound_attempt: usize,
) -> Option<UpstreamHealthReason> {
    if status == StatusCode::TOO_MANY_REQUESTS {
        return candidate_available.then_some(UpstreamHealthReason::RateLimited);
    }
    if status != StatusCode::SERVICE_UNAVAILABLE || !route.is_codex() {
        return None;
    }
    let transport_policy = runtime_transport_policy(&route.route.config, 0);
    let policy_permits =
        route.codex_store_disabled && transport_policy.service_unavailable_failover;
    let will_failover = policy_permits && candidate_available;
    tracing::warn!(
        %request_id,
        upstream_account_id = %route.route.account_id,
        candidate_rank,
        outbound_attempt,
        outbound_attempt_limit = PROXY_ROUTING_POLICY.max_attempts(),
        status = StatusCode::SERVICE_UNAVAILABLE.as_u16(),
        failure_kind = "unavailable",
        failover_policy = "service_unavailable",
        replay_contract = "codex_store_false",
        delivery_boundary = "not_started",
        policy_permits,
        candidate_available,
        decision = if will_failover { "failover" } else { "return_503" },
        transport_policy_source = transport_policy.source,
        stage = "upstream_status_failover_decision",
        "Codex service-unavailable response reached the bounded failover policy"
    );
    will_failover.then_some(UpstreamHealthReason::Unavailable)
}

pub(in crate::api::proxy) fn classify_attempt_failure(
    result: &Result<ProxyRouteResponse, ProxySendError>,
    rate_limit: Option<UpstreamFailureKind>,
) -> Option<(UpstreamFailureKind, UpstreamHealthReason)> {
    match result {
        Ok(result) if result.response.status() == StatusCode::TOO_MANY_REQUESTS => Some((
            rate_limit.unwrap_or(UpstreamFailureKind::RateLimited),
            UpstreamHealthReason::RateLimited,
        )),
        Ok(result) if retryable_upstream_status(result.response.status()) => Some((
            UpstreamFailureKind::Unavailable,
            UpstreamHealthReason::Unavailable,
        )),
        // A complete rejected response may mark the account unavailable, but
        // it remains non-replayable across accounts.
        Err(ProxySendError::RetryableCodexBadRequest) => Some((
            UpstreamFailureKind::Unavailable,
            UpstreamHealthReason::Unavailable,
        )),
        Err(ProxySendError::RetryableConnection(_)) => Some((
            UpstreamFailureKind::Connection,
            UpstreamHealthReason::Connection,
        )),
        Ok(_) => None,
        Err(
            ProxySendError::CodexBadRequest
            | ProxySendError::AmbiguousResponse(_)
            | ProxySendError::CandidateUnavailable
            | ProxySendError::NonRetryableTransport
            | ProxySendError::CredentialUnavailable
            | ProxySendError::Credential,
        ) => None,
    }
}
