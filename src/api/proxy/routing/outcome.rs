use super::*;
use crate::{db::UpstreamFailureKind, metrics::UpstreamHealthReason};

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
