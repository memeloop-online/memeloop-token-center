use std::{collections::BTreeMap, fmt::Write};

use super::Metrics;

/// Metric-only, fixed-cardinality projection of the native Codex 400 domain
/// classification. Transport code must not use this enum to make decisions.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum CodexBadRequestClassification {
    DefiniteTransient,
    DefiniteOrdinary,
    UnclassifiableContentType,
    UnclassifiableTooLarge,
    UnclassifiableTimedOut,
    UnclassifiableReadFailed,
    UnclassifiableInvalidJson,
}

impl CodexBadRequestClassification {
    const fn label(self) -> &'static str {
        match self {
            Self::DefiniteTransient => "retryable",
            Self::DefiniteOrdinary => "ordinary",
            Self::UnclassifiableContentType => "content_type",
            Self::UnclassifiableTooLarge => "too_large",
            Self::UnclassifiableTimedOut => "timed_out",
            Self::UnclassifiableReadFailed => "read_failed",
            Self::UnclassifiableInvalidJson => "invalid_json",
        }
    }
}

/// Lifecycle outcome of the one permitted same-account retry after a complete
/// native Codex HTTP 400. The result is recorded only after the downstream
/// response reaches a terminal state; labels contain no identities.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum CodexBadRequestRetry {
    Started,
    Succeeded,
    Failed,
    Cancelled,
}

impl CodexBadRequestRetry {
    const fn label(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl Metrics {
    pub(crate) fn observe_codex_bad_request_classification(
        &self,
        classification: CodexBadRequestClassification,
    ) {
        let mut values = self
            .inner
            .codex_bad_request_classifications
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let value = values.entry(classification).or_default();
        *value = value.saturating_add(1);
    }

    pub(crate) fn observe_codex_bad_request_retry(&self, outcome: CodexBadRequestRetry) {
        let mut values = self
            .inner
            .codex_bad_request_retries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let value = values.entry(outcome).or_default();
        *value = value.saturating_add(1);
    }
}

pub(super) fn render_bad_requests(
    output: &mut String,
    classifications: &BTreeMap<CodexBadRequestClassification, u64>,
    retries: &BTreeMap<CodexBadRequestRetry, u64>,
) {
    output.push_str("# HELP memeloop_token_center_codex_bad_request_classifications_total Native Codex HTTP 400 classifications with fixed, body-free labels.\n");
    output
        .push_str("# TYPE memeloop_token_center_codex_bad_request_classifications_total counter\n");
    for (classification, value) in classifications {
        let _ = writeln!(
            output,
            "memeloop_token_center_codex_bad_request_classifications_total{{classification=\"{}\"}} {value}",
            classification.label()
        );
    }
    output.push_str("# HELP memeloop_token_center_codex_bad_request_retries_total Bounded same-account retries after a complete native Codex HTTP 400.\n");
    output.push_str("# TYPE memeloop_token_center_codex_bad_request_retries_total counter\n");
    for (outcome, value) in retries {
        let _ = writeln!(
            output,
            "memeloop_token_center_codex_bad_request_retries_total{{outcome=\"{}\"}} {value}",
            outcome.label()
        );
    }
}
