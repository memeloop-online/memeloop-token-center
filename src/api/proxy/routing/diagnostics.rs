//! Host-owned evidence labels only. These observations never authorize replay.
use super::*;

fn classification(status: Option<StatusCode>, error: Option<&ProxySendError>) -> &'static str {
    match (status, error) {
        (Some(_), None) => "upstream_http_status",
        (None, Some(ProxySendError::RetryableConnection(_))) => "connect_not_delivered",
        (None, Some(ProxySendError::OuterDeadline)) => "outer_deadline_delivery_unknown",
        (None, Some(ProxySendError::NonRetryableTransport(kind))) => kind.diagnostic_outcome(),
        (None, Some(ProxySendError::AmbiguousResponse(_))) => "response_delivery_unknown",
        (None, Some(ProxySendError::CandidateUnavailable)) => "local_candidate_unavailable",
        (None, Some(ProxySendError::CredentialUnavailable | ProxySendError::Credential)) => {
            "local_credential"
        }
        (
            None,
            Some(ProxySendError::CodexBadRequest | ProxySendError::RetryableCodexBadRequest),
        ) => "upstream_rejected_request",
        _ => "inconsistent_evidence",
    }
}

pub(in crate::api::proxy) fn observe_send(
    request_id: Uuid,
    candidate_rank: usize,
    outbound_attempt: usize,
    result: &Result<ProxyRouteResponse, ProxySendError>,
) {
    let status = result
        .as_ref()
        .ok()
        .map(|response| response.response.status());
    let error = result.as_ref().err();
    emit(request_id, candidate_rank, outbound_attempt, status, error);
}

fn emit(
    request_id: Uuid,
    candidate_rank: usize,
    outbound_attempt: usize,
    status: Option<StatusCode>,
    error: Option<&ProxySendError>,
) {
    tracing::info!(
        %request_id, candidate_rank, outbound_attempt,
        stage = "upstream_attempt_observed",
        outcome = classification(status, error),
        status = status.map(|value| value.as_u16()),
        disposition = super::failover_disposition(status, error).as_str(),
        "proxy observed attempt delivery evidence"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Writer(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn actual_event_drops_error_details_and_has_only_allowlisted_fields() {
        let writer = Writer::default();
        let sink = writer.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_writer(move || sink.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            emit(
                Uuid::nil(),
                1,
                1,
                None,
                Some(&ProxySendError::AmbiguousResponse("SECRET_CANARY")),
            );
            emit(
                Uuid::nil(),
                1,
                1,
                Some(StatusCode::SERVICE_UNAVAILABLE),
                None,
            );
        });
        let bytes = writer.0.lock().unwrap();
        let logs = std::str::from_utf8(&bytes).unwrap();
        assert!(!logs.contains("SECRET_CANARY"));
        let events: Vec<Value> = logs
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 2);
        for event in &events {
            for field in event["fields"].as_object().unwrap().keys() {
                assert!(
                    [
                        "message",
                        "request_id",
                        "candidate_rank",
                        "outbound_attempt",
                        "stage",
                        "outcome",
                        "status",
                        "disposition"
                    ]
                    .contains(&field.as_str())
                );
            }
            assert_eq!(event["fields"]["disposition"], "no_replay_evidence");
        }
        assert_eq!(events[0]["fields"]["outcome"], "response_delivery_unknown");
        assert_eq!(events[1]["fields"]["status"], 503);
        assert_eq!(events[1]["fields"]["outcome"], "upstream_http_status");
    }

    #[test]
    fn unknown_delivery_is_distinct_from_local_and_explicit_response_evidence() {
        for (error, label) in [
            (
                ProxySendError::CandidateUnavailable,
                "local_candidate_unavailable",
            ),
            (ProxySendError::CredentialUnavailable, "local_credential"),
            (
                ProxySendError::RetryableConnection("SECRET_CANARY"),
                "connect_not_delivered",
            ),
            (
                ProxySendError::NonRetryableTransport(TransportFailureKind::ConnectionReset),
                "transport_connection_reset_delivery_unknown",
            ),
            (
                ProxySendError::OuterDeadline,
                "outer_deadline_delivery_unknown",
            ),
            (
                ProxySendError::AmbiguousResponse("SECRET_CANARY"),
                "response_delivery_unknown",
            ),
        ] {
            assert_eq!(classification(None, Some(&error)), label);
            assert!(!classification(None, Some(&error)).contains("SECRET_CANARY"));
        }
        for status in [200, 400, 429, 503, 504] {
            assert_eq!(
                classification(Some(StatusCode::from_u16(status).unwrap()), None),
                "upstream_http_status"
            );
        }
        assert_eq!(
            super::super::failover_disposition(None, Some(&ProxySendError::OuterDeadline)),
            super::super::outcome::FailoverDisposition::Stop
        );
    }

    #[test]
    fn transport_failure_labels_are_allowlisted_and_durable() {
        for (kind, outcome, error_code) in [
            (
                TransportFailureKind::Timeout,
                "transport_timeout_delivery_unknown",
                "upstream_transport_timeout",
            ),
            (
                TransportFailureKind::ConnectionReset,
                "transport_connection_reset_delivery_unknown",
                "upstream_transport_connection_reset",
            ),
            (
                TransportFailureKind::Body,
                "transport_body_delivery_unknown",
                "upstream_transport_body",
            ),
            (
                TransportFailureKind::Decode,
                "transport_decode_delivery_unknown",
                "upstream_transport_decode",
            ),
            (
                TransportFailureKind::Request,
                "transport_request_delivery_unknown",
                "upstream_transport_request",
            ),
            (
                TransportFailureKind::Other,
                "transport_other_delivery_unknown",
                "upstream_transport_other",
            ),
        ] {
            assert_eq!(kind.diagnostic_outcome(), outcome);
            assert_eq!(kind.error_code(), error_code);
            assert!(!outcome.contains("http"));
            assert!(!error_code.contains("http"));
        }
    }
}
