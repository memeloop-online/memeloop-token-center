use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthRefreshFailureKind {
    Configuration,
    Network,
    Timeout,
    HttpRejected,
    ResponseBody,
    ResponseDecode,
    ResponseValidation,
    InvalidGrant,
    RefreshRevoked,
    InvalidRefreshToken,
    LocalPersistence,
    Other,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthRefreshFailure {
    pub kind: OAuthRefreshFailureKind,
    pub http_status: Option<u16>,
}

impl OAuthRefreshFailure {
    pub(crate) fn new(kind: OAuthRefreshFailureKind, http_status: Option<u16>) -> Self {
        Self { kind, http_status }
    }

    pub(crate) fn requires_reauthorization(self) -> bool {
        matches!(
            self.kind,
            OAuthRefreshFailureKind::InvalidGrant
                | OAuthRefreshFailureKind::RefreshRevoked
                | OAuthRefreshFailureKind::InvalidRefreshToken
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OAuthRefreshOutcome {
    NotDispatched,
    OutcomeUnknown,
    ReauthorizationRequired,
    PendingLocal,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OAuthRefreshDiagnostic {
    pub version: u8,
    pub credential_generation: i64,
    pub attempt_created_at: i64,
    pub attempt_finished_at: i64,
    pub outcome: OAuthRefreshOutcome,
    pub failure: OAuthRefreshFailure,
}

impl OAuthRefreshDiagnostic {
    pub(crate) fn from_json(value: &str, generation: i64) -> Option<Self> {
        if value.len() > 2048 {
            return None;
        }
        let diagnostic: Self = serde_json::from_str(value).ok()?;
        if diagnostic.version != 1
            || diagnostic.credential_generation != generation
            || diagnostic.attempt_created_at < 0
            || diagnostic.attempt_finished_at < diagnostic.attempt_created_at
            || diagnostic
                .failure
                .http_status
                .is_some_and(|status| !(100..=599).contains(&status))
            || (diagnostic.outcome == OAuthRefreshOutcome::ReauthorizationRequired
                && !diagnostic.failure.requires_reauthorization())
        {
            return None;
        }
        Some(diagnostic)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OAuthAccessState {
    Valid,
    Expired,
    Revoked,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OAuthRefreshState {
    NotObserved,
    InProgress,
    PendingLocal,
    OutcomeUnknown,
    Failed,
    ReauthorizationRequired,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct OAuthRefreshStatus {
    pub credential_generation: i64,
    pub access_state: OAuthAccessState,
    pub refresh_state: OAuthRefreshState,
    pub attempt_created_at: Option<i64>,
    pub request_started_at: Option<i64>,
    pub attempt_finished_at: Option<i64>,
    pub failure_class: Option<OAuthRefreshFailureKind>,
    pub http_status: Option<u16>,
    pub reauthorization_required: bool,
}

pub(crate) struct OAuthRefreshMetadata {
    pub generation: i64,
    pub expires_at: Option<i64>,
    pub revoked_at: Option<i64>,
    pub lease_created_at: Option<i64>,
    pub lease_expires_at: Option<i64>,
    pub request_started_at: Option<i64>,
    pub has_pending: bool,
    pub diagnostic_json: Option<String>,
}

impl OAuthRefreshMetadata {
    pub(crate) fn status(self, now: i64) -> OAuthRefreshStatus {
        let diagnostic = self
            .diagnostic_json
            .as_deref()
            .and_then(|value| OAuthRefreshDiagnostic::from_json(value, self.generation))
            .filter(|diagnostic| {
                self.lease_created_at
                    .is_none_or(|created| created == diagnostic.attempt_created_at)
            });
        let refresh_state = if self.has_pending {
            OAuthRefreshState::PendingLocal
        } else if let Some(diagnostic) = &diagnostic {
            match diagnostic.outcome {
                OAuthRefreshOutcome::ReauthorizationRequired => {
                    OAuthRefreshState::ReauthorizationRequired
                }
                OAuthRefreshOutcome::OutcomeUnknown | OAuthRefreshOutcome::PendingLocal => {
                    OAuthRefreshState::OutcomeUnknown
                }
                OAuthRefreshOutcome::NotDispatched => OAuthRefreshState::Failed,
            }
        } else if self.lease_expires_at.is_some_and(|expiry| expiry > now) {
            OAuthRefreshState::InProgress
        } else if self.request_started_at.is_some() {
            OAuthRefreshState::OutcomeUnknown
        } else {
            OAuthRefreshState::NotObserved
        };
        OAuthRefreshStatus {
            credential_generation: self.generation,
            access_state: if self.revoked_at.is_some() {
                OAuthAccessState::Revoked
            } else {
                match self.expires_at {
                    Some(expiry) if expiry <= now => OAuthAccessState::Expired,
                    Some(_) => OAuthAccessState::Valid,
                    None => OAuthAccessState::Unknown,
                }
            },
            refresh_state,
            attempt_created_at: self
                .lease_created_at
                .or_else(|| diagnostic.as_ref().map(|value| value.attempt_created_at)),
            request_started_at: self.request_started_at,
            attempt_finished_at: diagnostic.as_ref().map(|value| value.attempt_finished_at),
            failure_class: diagnostic.as_ref().map(|value| value.failure.kind),
            http_status: diagnostic
                .as_ref()
                .and_then(|value| value.failure.http_status),
            reauthorization_required: refresh_state == OAuthRefreshState::ReauthorizationRequired,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> OAuthRefreshMetadata {
        OAuthRefreshMetadata {
            generation: 7,
            expires_at: Some(10),
            revoked_at: None,
            lease_created_at: Some(5),
            lease_expires_at: Some(20),
            request_started_at: Some(6),
            has_pending: false,
            diagnostic_json: None,
        }
    }

    fn diagnostic() -> OAuthRefreshDiagnostic {
        OAuthRefreshDiagnostic {
            version: 1,
            credential_generation: 7,
            attempt_created_at: 5,
            attempt_finished_at: 9,
            outcome: OAuthRefreshOutcome::ReauthorizationRequired,
            failure: OAuthRefreshFailure::new(OAuthRefreshFailureKind::InvalidGrant, Some(400)),
        }
    }

    #[test]
    fn expired_access_and_old_started_attempt_do_not_prove_invalid_refresh() {
        let status = metadata().status(30);
        assert_eq!(status.access_state, OAuthAccessState::Expired);
        assert_eq!(status.refresh_state, OAuthRefreshState::OutcomeUnknown);
        assert!(!status.reauthorization_required);
        assert!(status.attempt_finished_at.is_none());
        assert!(status.failure_class.is_none());
    }

    #[test]
    fn explicit_rejection_is_separate_from_access_expiry_and_pending_result() {
        let mut source = metadata();
        source.expires_at = Some(100);
        source.diagnostic_json = Some(serde_json::to_string(&diagnostic()).unwrap());
        let status = source.status(30);
        assert_eq!(status.access_state, OAuthAccessState::Valid);
        assert!(status.reauthorization_required);
        let mut pending = metadata();
        pending.has_pending = true;
        pending.diagnostic_json = Some(serde_json::to_string(&diagnostic()).unwrap());
        let status = pending.status(30);
        assert_eq!(status.refresh_state, OAuthRefreshState::PendingLocal);
        assert!(!status.reauthorization_required);
    }

    #[test]
    fn previous_attempt_and_generation_diagnostics_are_not_current_evidence() {
        for (generation, created_at) in [(6, 5), (7, 4)] {
            let mut previous = diagnostic();
            previous.credential_generation = generation;
            previous.attempt_created_at = created_at;
            let mut source = metadata();
            source.diagnostic_json = Some(serde_json::to_string(&previous).unwrap());
            let status = source.status(30);
            assert_eq!(status.refresh_state, OAuthRefreshState::OutcomeUnknown);
            assert!(status.failure_class.is_none());
            assert!(!status.reauthorization_required);
        }
    }

    #[test]
    fn malformed_or_unrecognized_diagnostics_cannot_create_success_or_rejection() {
        for value in ["{}", "invalid-json", "{\"refresh_token\":\"fixture-only\"}"] {
            let mut source = metadata();
            source.diagnostic_json = Some(value.to_owned());
            let status = source.status(30);
            assert_eq!(status.refresh_state, OAuthRefreshState::OutcomeUnknown);
            assert!(status.failure_class.is_none());
            assert!(
                !serde_json::to_string(&status)
                    .unwrap()
                    .contains("fixture-only")
            );
        }
    }

    #[test]
    fn no_attempt_is_not_an_observed_refresh_success() {
        let mut source = metadata();
        source.lease_created_at = None;
        source.lease_expires_at = None;
        source.request_started_at = None;
        source.expires_at = None;
        let status = source.status(30);
        assert_eq!(status.refresh_state, OAuthRefreshState::NotObserved);
        assert_eq!(status.access_state, OAuthAccessState::Unknown);
        assert!(!status.reauthorization_required);
    }
}
