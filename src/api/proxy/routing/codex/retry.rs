use crate::metrics::{CodexBadRequestClassification, CodexBadRequestRetry, Metrics};

use super::super::super::codex_transport;
use super::super::ProxySendError;

/// Typed state for the one replay which Codex's non-persistent request
/// contract permits. `Retried` is terminal for replay decisions, so no caller
/// can accidentally issue a third attempt or switch accounts from this path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CodexRetryState {
    Initial { retry_permitted: bool },
    Retried,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AttemptControl {
    RetrySameAccount,
    Return(ProxySendError),
}

impl CodexRetryState {
    pub(super) const fn new(retry_permitted: bool) -> Self {
        Self::Initial { retry_permitted }
    }

    pub(super) fn after_bad_request(
        &mut self,
        disposition: codex_transport::BadRequestDisposition,
    ) -> AttemptControl {
        let error = match disposition {
            codex_transport::BadRequestDisposition::DefiniteTransient => {
                ProxySendError::RetryableCodexBadRequest
            }
            codex_transport::BadRequestDisposition::DefiniteOrdinary
            | codex_transport::BadRequestDisposition::Unclassifiable(_) => {
                ProxySendError::CodexBadRequest
            }
        };
        let replay_permitted = matches!(
            self,
            Self::Initial {
                retry_permitted: true
            }
        );
        let definite_rejection = matches!(
            disposition,
            codex_transport::BadRequestDisposition::DefiniteTransient
                | codex_transport::BadRequestDisposition::DefiniteOrdinary
        );
        if replay_permitted && definite_rejection {
            *self = Self::Retried;
            AttemptControl::RetrySameAccount
        } else {
            AttemptControl::Return(error)
        }
    }

    pub(super) const fn outcome(self) -> CodexRetryOutcome {
        match self {
            Self::Initial { .. } => CodexRetryOutcome::NotRetried,
            Self::Retried => CodexRetryOutcome::Retried,
        }
    }
}

/// The retry outcome is carried with the admitted response. Only buffered or
/// streaming terminalization can settle it because receiving a 2xx header or
/// admitting an SSE frame does not prove a successful Responses completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::api::proxy) enum CodexRetryOutcome {
    NotRetried,
    Retried,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::api::proxy) enum CodexRetryTerminal {
    Succeeded,
    Failed,
    Cancelled,
}

impl CodexRetryOutcome {
    pub(super) fn observe_terminal(self, metrics: &Metrics, terminal: CodexRetryTerminal) {
        if self == Self::Retried {
            metrics.observe_codex_bad_request_retry(match terminal {
                CodexRetryTerminal::Succeeded => CodexBadRequestRetry::Succeeded,
                CodexRetryTerminal::Failed => CodexBadRequestRetry::Failed,
                CodexRetryTerminal::Cancelled => CodexBadRequestRetry::Cancelled,
            });
        }
    }
}

/// Owns the one terminal observation for an admitted Codex retry. The guard
/// moves with the response and defaults to `Failed` if a route is dropped, a
/// task is cancelled, or its future unwinds before explicit settlement.
#[must_use]
pub(in crate::api::proxy) struct CodexRetryTerminalGuard {
    metrics: Option<Metrics>,
    outcome: CodexRetryOutcome,
}

impl CodexRetryTerminalGuard {
    pub(in crate::api::proxy) fn new(metrics: Metrics, outcome: CodexRetryOutcome) -> Self {
        Self {
            metrics: (outcome == CodexRetryOutcome::Retried).then_some(metrics),
            outcome,
        }
    }

    pub(in crate::api::proxy) const fn inactive() -> Self {
        Self {
            metrics: None,
            outcome: CodexRetryOutcome::NotRetried,
        }
    }

    pub(in crate::api::proxy) fn complete(&mut self, terminal: CodexRetryTerminal) {
        if let Some(metrics) = self.metrics.take() {
            self.outcome.observe_terminal(&metrics, terminal);
        }
    }
}

impl Drop for CodexRetryTerminalGuard {
    fn drop(&mut self) {
        self.complete(CodexRetryTerminal::Failed);
    }
}

/// One-way adapter from a transport-domain assessment to metric labels. It is
/// deliberately outside `codex_transport`: routing may observe a decision,
/// but telemetry cannot influence the decision's replay semantics.
pub(super) fn observe_bad_request_disposition(
    metrics: &Metrics,
    disposition: codex_transport::BadRequestDisposition,
) {
    let classification = match disposition {
        codex_transport::BadRequestDisposition::DefiniteTransient => {
            CodexBadRequestClassification::DefiniteTransient
        }
        codex_transport::BadRequestDisposition::DefiniteOrdinary => {
            CodexBadRequestClassification::DefiniteOrdinary
        }
        codex_transport::BadRequestDisposition::Unclassifiable(reason) => match reason {
            codex_transport::BadRequestUnclassifiableReason::ContentType => {
                CodexBadRequestClassification::UnclassifiableContentType
            }
            codex_transport::BadRequestUnclassifiableReason::TooLarge => {
                CodexBadRequestClassification::UnclassifiableTooLarge
            }
            codex_transport::BadRequestUnclassifiableReason::TimedOut => {
                CodexBadRequestClassification::UnclassifiableTimedOut
            }
            codex_transport::BadRequestUnclassifiableReason::ReadFailed => {
                CodexBadRequestClassification::UnclassifiableReadFailed
            }
            codex_transport::BadRequestUnclassifiableReason::InvalidJson => {
                CodexBadRequestClassification::UnclassifiableInvalidJson
            }
        },
    };
    metrics.observe_codex_bad_request_classification(classification);
}

#[cfg(test)]
mod tests {
    use super::super::super::super::lifecycle::run_bounded_proxy_lifecycle;
    use super::*;

    #[test]
    fn definite_rejections_have_one_same_account_transition() {
        let mut retry = CodexRetryState::new(true);
        assert_eq!(
            retry.after_bad_request(codex_transport::BadRequestDisposition::DefiniteTransient),
            AttemptControl::RetrySameAccount
        );
        assert_eq!(retry.outcome(), CodexRetryOutcome::Retried);
        assert_eq!(
            retry.after_bad_request(codex_transport::BadRequestDisposition::DefiniteOrdinary),
            AttemptControl::Return(ProxySendError::CodexBadRequest)
        );
    }

    #[test]
    fn unclassifiable_400_never_enters_replay() {
        let mut retry = CodexRetryState::new(true);
        assert_eq!(
            retry.after_bad_request(codex_transport::BadRequestDisposition::Unclassifiable(
                codex_transport::BadRequestUnclassifiableReason::InvalidJson,
            )),
            AttemptControl::Return(ProxySendError::CodexBadRequest)
        );
        assert_eq!(retry.outcome(), CodexRetryOutcome::NotRetried);
    }

    #[test]
    fn disabled_store_contract_disables_replay() {
        let mut retry = CodexRetryState::new(false);
        assert_eq!(
            retry.after_bad_request(codex_transport::BadRequestDisposition::DefiniteTransient),
            AttemptControl::Return(ProxySendError::RetryableCodexBadRequest)
        );
        assert_eq!(retry.outcome(), CodexRetryOutcome::NotRetried);
    }

    #[tokio::test(start_paused = true)]
    async fn lifecycle_deadline_drops_a_retried_guard_as_failed_once() {
        let metrics = Metrics::default();
        metrics.observe_codex_bad_request_retry(CodexBadRequestRetry::Started);
        let guard = CodexRetryTerminalGuard::new(metrics.clone(), CodexRetryOutcome::Retried);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        let task = tokio::spawn(async move {
            run_bounded_proxy_lifecycle(deadline, async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            })
            .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        assert!(task.await.unwrap().is_err());
        let rendered = metrics.render(&crate::metrics::RuntimeMetrics::default());
        assert!(rendered.contains(
            "memeloop_token_center_codex_bad_request_retries_total{outcome=\"failed\"} 1"
        ));
        assert!(!rendered.contains(
            "memeloop_token_center_codex_bad_request_retries_total{outcome=\"succeeded\"} 1"
        ));
    }
}
