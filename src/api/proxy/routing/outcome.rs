use super::*;
use crate::{db::UpstreamFailureKind, metrics::UpstreamHealthReason};

/// Health failure and permission to replay are deliberately separate facts.
/// In particular, a dispatched 503 may cool an account but grants no replay.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::api::proxy) enum FailoverDisposition {
    CandidateNotDispatched,
    ConnectionNotDelivered,
    RateLimitRejected,
    Stop,
}

impl FailoverDisposition {
    pub(in crate::api::proxy) fn reason(&self) -> Option<UpstreamHealthReason> {
        match self {
            Self::CandidateNotDispatched => Some(UpstreamHealthReason::Unavailable),
            Self::ConnectionNotDelivered => Some(UpstreamHealthReason::Connection),
            Self::RateLimitRejected => Some(UpstreamHealthReason::RateLimited),
            Self::Stop => None,
        }
    }

    pub(in crate::api::proxy) fn as_str(&self) -> &'static str {
        match self {
            Self::CandidateNotDispatched => "candidate_not_dispatched",
            Self::ConnectionNotDelivered => "connection_not_delivered",
            Self::RateLimitRejected => "rate_limit_rejected",
            Self::Stop => "no_replay_evidence",
        }
    }
}

pub(in crate::api::proxy) fn failover_disposition(
    status: Option<StatusCode>,
    error: Option<&ProxySendError>,
) -> FailoverDisposition {
    match (status, error) {
        (
            None,
            Some(ProxySendError::CandidateUnavailable | ProxySendError::CredentialUnavailable),
        ) => FailoverDisposition::CandidateNotDispatched,
        (None, Some(ProxySendError::RetryableConnection(_))) => {
            FailoverDisposition::ConnectionNotDelivered
        }
        (Some(StatusCode::TOO_MANY_REQUESTS), None) => FailoverDisposition::RateLimitRejected,
        _ => FailoverDisposition::Stop,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_non_delivery_or_rate_limit_rejection_permits_failover() {
        assert_eq!(
            failover_disposition(Some(StatusCode::TOO_MANY_REQUESTS), None),
            FailoverDisposition::RateLimitRejected
        );
        assert_eq!(
            failover_disposition(
                None,
                Some(&ProxySendError::RetryableConnection("proxy_connect"))
            ),
            FailoverDisposition::ConnectionNotDelivered
        );
        for status in [200, 400, 401, 403, 500, 502, 503, 504] {
            assert_eq!(
                failover_disposition(Some(StatusCode::from_u16(status).unwrap()), None),
                FailoverDisposition::Stop
            );
        }
        for error in [
            ProxySendError::NonRetryableTransport,
            ProxySendError::AmbiguousResponse("visible_output"),
            ProxySendError::RetryableCodexBadRequest,
            ProxySendError::CodexBadRequest,
            ProxySendError::Credential,
        ] {
            assert_eq!(
                failover_disposition(None, Some(&error)),
                FailoverDisposition::Stop
            );
        }
        assert_eq!(
            failover_disposition(
                Some(StatusCode::SERVICE_UNAVAILABLE),
                Some(&ProxySendError::RetryableConnection("connect"))
            ),
            FailoverDisposition::Stop
        );
    }
}
