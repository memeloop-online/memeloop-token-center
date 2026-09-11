use super::*;

/// All data needed after downstream delivery has ended. The delivery pump owns
/// the response body, while this typed boundary owns settlement and the two
/// route health guards.
pub(super) struct StreamingFinalizationInput<'a> {
    pub(super) state: &'a AppState,
    pub(super) status_code: i64,
    pub(super) protocol: Protocol,
    pub(super) is_codex_route: bool,
    pub(super) codex_retry: CodexRetryTerminalGuard,
    pub(super) upstream_attempt: UpstreamAttemptGuard,
    pub(super) request_id: Uuid,
    pub(super) reservation: crate::model::UsageReservation,
    pub(super) started: Instant,
    pub(super) input_token_ceiling: i64,
    pub(super) output_token_ceiling: i64,
    pub(super) requested_service_tier: Option<String>,
    pub(super) conversation: Option<ProxyConversation>,
    pub(super) tenant_id: Uuid,
    pub(super) transport_error: Option<&'static str>,
    pub(super) delivered_billable: bool,
    pub(super) sse_summary: Option<ResponsesSseSummary>,
    pub(super) usage_capture: Vec<u8>,
    pub(super) response_archive_attempt: Option<crate::proxy_lifecycle::ProxyArchiveAttempt>,
    pub(super) stored_response: String,
    pub(super) gap_response: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StreamingUpstreamEvidence {
    Succeeded,
    Inconclusive,
    InvalidResponse,
}

fn streaming_upstream_evidence(
    terminal_result_failed: bool,
    transport_error: Option<&str>,
    sse_summary: Option<&ResponsesSseSummary>,
    error_code: Option<&str>,
) -> StreamingUpstreamEvidence {
    if sse_summary.is_some_and(|summary| summary.observed_protocol_invalid) {
        return StreamingUpstreamEvidence::InvalidResponse;
    }
    if matches!(
        transport_error,
        Some(
            "upstream_stream"
                | "upstream_timeout"
                | "upstream_response_event_batch_too_large"
                | "downstream_disconnected"
                | "downstream_backpressure"
                | "delivery_state"
        )
    ) {
        return StreamingUpstreamEvidence::Inconclusive;
    }
    if sse_summary.is_some_and(|summary| summary.protocol_invalid) {
        return StreamingUpstreamEvidence::InvalidResponse;
    }
    if matches!(
        sse_summary.map(|summary| &summary.outcome),
        Some(ResponsesSseOutcome::Failed)
    ) {
        return StreamingUpstreamEvidence::Inconclusive;
    }
    if error_code.is_some() {
        StreamingUpstreamEvidence::InvalidResponse
    } else if terminal_result_failed {
        StreamingUpstreamEvidence::Inconclusive
    } else {
        StreamingUpstreamEvidence::Succeeded
    }
}

pub(super) async fn finalize_streaming_lifecycle(input: StreamingFinalizationInput<'_>) {
    let StreamingFinalizationInput {
        state,
        status_code,
        protocol,
        is_codex_route,
        mut codex_retry,
        mut upstream_attempt,
        request_id,
        reservation,
        started,
        input_token_ceiling,
        output_token_ceiling,
        requested_service_tier,
        conversation,
        tenant_id,
        transport_error,
        delivered_billable,
        sse_summary,
        usage_capture,
        response_archive_attempt,
        stored_response,
        gap_response,
    } = input;
    let protocol_error = match sse_summary.as_ref().map(|summary| &summary.outcome) {
        Some(ResponsesSseOutcome::Failed) => Some("upstream_failed_response"),
        Some(ResponsesSseOutcome::Incomplete) => Some("upstream_incomplete_response"),
        Some(ResponsesSseOutcome::Completed { .. }) | None => None,
    };
    let (mut terminal_status, mut error_code) = match transport_error {
        // 499 is an operator receipt for a downstream that closed its body.
        // It is never sent on the wire because the HTTP response was already
        // admitted, but it keeps client cancellation out of upstream 5xx
        // availability metrics and request history.
        Some("downstream_disconnected") => (499, Some("client_cancelled")),
        // A live but persistently unread downstream is also local evidence,
        // not an upstream failure.
        Some("downstream_backpressure") => (504, Some("downstream_backpressure")),
        // Delivery state is owned by this service's database. Classify its
        // failure as internal while retaining the stable diagnostic code.
        Some("delivery_state") => (500, Some("delivery_state")),
        Some(error) => (502, Some(error)),
        None => match protocol_error {
            Some(error) => (502, Some(error)),
            None => (status_code, None),
        },
    };
    let full_contract_usage = || TokenUsage {
        input_tokens: input_token_ceiling,
        output_tokens: output_token_ceiling,
        ..TokenUsage::default()
    };
    let mut charge_contract_ceiling = delivered_billable && error_code.is_some();
    let mut usage = if error_code.is_some() {
        if delivered_billable {
            full_contract_usage()
        } else {
            TokenUsage::default()
        }
    } else {
        let extracted_usage = match sse_summary.as_ref() {
            Some(summary) if summary.usage_invalid => ExtractedUsage::Invalid,
            Some(summary) => summary.usage.clone().map_or_else(
                || {
                    if is_codex_route {
                        ExtractedUsage::Invalid
                    } else {
                        ExtractedUsage::Missing
                    }
                },
                ExtractedUsage::Valid,
            ),
            None => extract_usage_checked(&usage_capture),
        };
        match extracted_usage {
            ExtractedUsage::Valid(usage) => usage,
            ExtractedUsage::Missing => {
                charge_contract_ceiling = delivered_billable;
                if delivered_billable {
                    full_contract_usage()
                } else {
                    TokenUsage::default()
                }
            }
            ExtractedUsage::Invalid => {
                terminal_status = 502;
                error_code = Some("upstream_invalid_usage");
                charge_contract_ceiling = delivered_billable;
                if delivered_billable {
                    full_contract_usage()
                } else {
                    TokenUsage::default()
                }
            }
        }
    };
    match crate::db::normalize_proxy_usage(
        &usage,
        input_token_ceiling,
        output_token_ceiling,
        requested_service_tier.as_deref(),
    ) {
        Ok(normalized) => usage = normalized,
        Err(AppError::Upstream(_)) => {
            terminal_status = 502;
            error_code = Some("upstream_invalid_usage");
            charge_contract_ceiling = delivered_billable;
            usage = if delivered_billable {
                full_contract_usage()
            } else {
                TokenUsage::default()
            };
        }
        Err(_) => {
            terminal_status = 502;
            error_code = Some("upstream_invalid_usage");
            charge_contract_ceiling = delivered_billable;
            usage = if delivered_billable {
                full_contract_usage()
            } else {
                TokenUsage::default()
            };
        }
    }
    let response_id =
        if (200..400).contains(&terminal_status) && matches!(protocol, Protocol::OpenAiResponses) {
            match sse_summary.as_ref().map(|summary| &summary.outcome) {
                Some(ResponsesSseOutcome::Completed { response_id }) => response_id.clone(),
                None => extract_response_id(&usage_capture),
                Some(ResponsesSseOutcome::Failed | ResponsesSseOutcome::Incomplete) => None,
            }
        } else {
            None
        };
    // A retry's success is a protocol-terminal property, not a 2xx header or
    // SSE framing property. Direct Codex streams must have exactly one
    // matching `response.completed` carrying a stable response id; capture
    // marks duplicate/mismatched terminal events incomplete before this point.
    let retry_terminal = if is_codex_route
        && error_code.is_none()
        && (200..400).contains(&terminal_status)
        && matches!(
            sse_summary.as_ref().map(|summary| &summary.outcome),
            Some(ResponsesSseOutcome::Completed {
                response_id: Some(_)
            })
        ) {
        CodexRetryTerminal::Succeeded
    } else if matches!(
        transport_error,
        Some("downstream_disconnected" | "downstream_backpressure")
    ) {
        CodexRetryTerminal::Cancelled
    } else {
        CodexRetryTerminal::Failed
    };
    let conversation_input = conversation
        .as_ref()
        .map(|conversation| ProxyConversationInput {
            key: &conversation.key,
            request_json: &conversation.request_json,
            hints: &conversation.hints,
            client_name: conversation.client_name.as_deref(),
            upstream_response_id: response_id.as_deref(),
        });
    let terminal_result = finish_proxy_request_with_archive_fallback(
        &state.db,
        FinishProxyRequest {
            request_id,
            tenant_id,
            reservation: &reservation,
            input_token_ceiling,
            output_token_ceiling,
            requested_service_tier: requested_service_tier.as_deref(),
            status_code: terminal_status,
            duration_ms: started.elapsed().as_millis() as i64,
            usage,
            charge_contract_ceiling,
            error_code,
            response_object: &stored_response,
            conversation: conversation_input,
        },
        response_archive_attempt.as_ref(),
        &gap_response,
    )
    .await;
    let terminal_result_failed = terminal_result.is_err();
    if terminal_result_failed {
        // The commit can be durable even when its acknowledgement is lost.
        // Preserve this request-scoped archive until its database owner is
        // known; deleting it here could leave a committed row dangling.
        tracing::error!(%request_id, stage = "terminal_transaction", "proxy request finalization failed");
    }
    let attempt_terminal = match streaming_upstream_evidence(
        terminal_result_failed,
        transport_error,
        sse_summary.as_ref(),
        error_code,
    ) {
        StreamingUpstreamEvidence::Succeeded => UpstreamAttemptTerminal::Succeeded,
        StreamingUpstreamEvidence::Inconclusive => UpstreamAttemptTerminal::Inconclusive,
        StreamingUpstreamEvidence::InvalidResponse => UpstreamAttemptTerminal::invalid_response(),
    };
    upstream_attempt.complete(attempt_terminal).await;
    codex_retry.complete(if terminal_result_failed {
        CodexRetryTerminal::Failed
    } else {
        retry_terminal
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(
        outcome: ResponsesSseOutcome,
        observed_protocol_invalid: bool,
        protocol_invalid: bool,
    ) -> ResponsesSseSummary {
        ResponsesSseSummary {
            outcome,
            usage: None,
            usage_invalid: false,
            observed_protocol_invalid,
            protocol_invalid,
        }
    }

    #[test]
    fn valid_failed_response_is_request_scoped_inconclusive_evidence() {
        let failed = summary(ResponsesSseOutcome::Failed, false, false);
        assert_eq!(
            streaming_upstream_evidence(
                false,
                None,
                Some(&failed),
                Some("upstream_failed_response")
            ),
            StreamingUpstreamEvidence::Inconclusive
        );
    }

    #[test]
    fn protocol_invalid_failure_survives_terminal_settlement_failure() {
        let invalid_failed = summary(ResponsesSseOutcome::Failed, true, true);
        assert_eq!(
            streaming_upstream_evidence(
                true,
                None,
                Some(&invalid_failed),
                Some("upstream_failed_response")
            ),
            StreamingUpstreamEvidence::InvalidResponse
        );
    }

    #[test]
    fn local_event_batch_boundary_is_inconclusive_evidence() {
        assert_eq!(
            streaming_upstream_evidence(
                false,
                Some("upstream_response_event_batch_too_large"),
                None,
                Some("upstream_response_event_batch_too_large")
            ),
            StreamingUpstreamEvidence::Inconclusive
        );
    }

    #[test]
    fn ambiguous_transport_dominates_its_derived_incomplete_capture_state() {
        let incomplete = summary(ResponsesSseOutcome::Incomplete, false, true);
        assert_eq!(
            streaming_upstream_evidence(
                false,
                Some("upstream_stream"),
                Some(&incomplete),
                Some("upstream_stream")
            ),
            StreamingUpstreamEvidence::Inconclusive
        );
    }

    #[test]
    fn observed_protocol_violation_precedes_a_later_ambiguous_transport_failure() {
        let invalid_failed = summary(ResponsesSseOutcome::Failed, true, true);
        assert_eq!(
            streaming_upstream_evidence(
                false,
                Some("upstream_stream"),
                Some(&invalid_failed),
                Some("upstream_stream")
            ),
            StreamingUpstreamEvidence::InvalidResponse
        );
    }

    #[test]
    fn observed_invalid_usage_precedes_a_later_ambiguous_transport_failure() {
        let mut invalid_usage = summary(ResponsesSseOutcome::Incomplete, true, true);
        invalid_usage.usage_invalid = true;
        assert_eq!(
            streaming_upstream_evidence(
                false,
                Some("upstream_stream"),
                Some(&invalid_usage),
                Some("upstream_stream")
            ),
            StreamingUpstreamEvidence::InvalidResponse
        );
    }

    #[test]
    fn settlement_failure_changes_an_otherwise_valid_success_to_inconclusive() {
        let completed = summary(
            ResponsesSseOutcome::Completed {
                response_id: Some("resp-valid".to_owned()),
            },
            false,
            false,
        );
        assert_eq!(
            streaming_upstream_evidence(true, None, Some(&completed), None),
            StreamingUpstreamEvidence::Inconclusive
        );
    }
}
