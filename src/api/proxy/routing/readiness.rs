use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CandidateCompatibility {
    Compatible,
    ProtocolMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PreparedRouteReadiness {
    Ready,
    Unavailable,
}

impl PreparedRouteReadiness {
    pub(super) const fn error_code(self) -> &'static str {
        match self {
            Self::Ready | Self::Unavailable => "upstream_credential_unavailable",
        }
    }
}

/// Reject candidate-specific protocol mismatches before any component prepare
/// hook can observe the request. Other preparation errors remain explicit:
/// they may follow an externally effectful component hook and are not safe to
/// hide through failover.
pub(super) fn candidate_compatibility(
    protocol: Protocol,
    route: &ResolvedUpstream,
) -> CandidateCompatibility {
    if codex_transport::is_driver(&route.driver) && !matches!(protocol, Protocol::OpenAiResponses) {
        CandidateCompatibility::ProtocolMismatch
    } else {
        CandidateCompatibility::Compatible
    }
}

/// Revalidate the exact account revision and credential generation used by
/// request preparation, then replace every transport field from one atomic DB
/// snapshot. A rotation or reconfiguration is a normal readiness transition;
/// the caller may safely try another already-prepared candidate.
pub(super) async fn refresh_route_snapshot(
    state: &AppState,
    route: &mut ResolvedUpstream,
) -> Result<PreparedRouteReadiness, AppError> {
    let expected_revision = route.transport_revision;
    let expected_generation = route.credential_generation;
    let Some(snapshot) = state
        .db
        .reload_prepared_upstream_snapshot(
            route.account_id,
            expected_revision,
            expected_generation,
            state.config.key_pepper.as_bytes(),
        )
        .await?
    else {
        return Ok(PreparedRouteReadiness::Unavailable);
    };
    if snapshot.transport_revision != expected_revision
        || snapshot.credential_generation != expected_generation
        || snapshot.driver != route.driver
        || snapshot.base_url != route.base_url
        || snapshot.config != route.config
    {
        return Ok(PreparedRouteReadiness::Unavailable);
    }
    route.transport_revision = snapshot.transport_revision;
    route.credential_generation = snapshot.credential_generation;
    route.driver = snapshot.driver;
    route.base_url = snapshot.base_url;
    route.config = snapshot.config;
    route.credential = snapshot.credential;
    Ok(PreparedRouteReadiness::Ready)
}

/// Credential expiry at the exact header-application timestamp is a readiness
/// transition, not evidence that the upstream failed. All other local
/// credential errors remain explicit configuration failures.
pub(super) fn credential_application_error(
    credential: &UpstreamCredential,
    now: i64,
) -> ProxySendError {
    if credential
        .expires_at()
        .is_some_and(|expires_at| expires_at <= now)
    {
        ProxySendError::CredentialUnavailable
    } else {
        ProxySendError::Credential
    }
}
