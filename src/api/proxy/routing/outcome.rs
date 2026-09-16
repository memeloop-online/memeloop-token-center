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
        Ok(result) => classify_response_failure(result.response.status(), rate_limit),
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
        // Ambiguous delivery forbids replaying this POST, but the transport
        // failure is still account-health evidence for independent requests.
        Err(ProxySendError::NonRetryableTransport(_)) => Some((
            UpstreamFailureKind::Connection,
            UpstreamHealthReason::Connection,
        )),
        Err(
            ProxySendError::CodexBadRequest
            | ProxySendError::AmbiguousResponse(_)
            | ProxySendError::CandidateUnavailable
            | ProxySendError::OuterDeadline
            | ProxySendError::CredentialUnavailable
            | ProxySendError::Credential,
        ) => None,
    }
}

fn classify_response_failure(
    status: StatusCode,
    rate_limit: Option<UpstreamFailureKind>,
) -> Option<(UpstreamFailureKind, UpstreamHealthReason)> {
    match status {
        StatusCode::TOO_MANY_REQUESTS => Some((
            rate_limit.unwrap_or(UpstreamFailureKind::RateLimited),
            UpstreamHealthReason::RateLimited,
        )),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Some((
            UpstreamFailureKind::Authentication,
            UpstreamHealthReason::Unavailable,
        )),
        status if retryable_upstream_status(status) => Some((
            UpstreamFailureKind::Unavailable,
            UpstreamHealthReason::Unavailable,
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authentication_is_hard_health_evidence_but_never_replay_permission() {
        for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
            assert!(matches!(
                classify_response_failure(status, None),
                Some((UpstreamFailureKind::Authentication, _))
            ));
            assert_eq!(
                failover_disposition(Some(status), None),
                FailoverDisposition::Stop
            );
        }
        assert!(classify_response_failure(StatusCode::BAD_REQUEST, None).is_none());
        assert!(matches!(
            classify_response_failure(StatusCode::SERVICE_UNAVAILABLE, None),
            Some((UpstreamFailureKind::Unavailable, _))
        ));
    }

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
            ProxySendError::NonRetryableTransport(TransportFailureKind::Other),
            ProxySendError::OuterDeadline,
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

    #[test]
    fn ambiguous_transport_failure_cools_account_without_permitting_replay() {
        let result = Err(ProxySendError::NonRetryableTransport(
            TransportFailureKind::ConnectionReset,
        ));
        assert_eq!(
            classify_attempt_failure(&result, None),
            Some((
                UpstreamFailureKind::Connection,
                UpstreamHealthReason::Connection
            ))
        );
        assert_eq!(
            failover_disposition(
                None,
                Some(&ProxySendError::NonRetryableTransport(
                    TransportFailureKind::ConnectionReset,
                )),
            ),
            FailoverDisposition::Stop
        );
    }
}
