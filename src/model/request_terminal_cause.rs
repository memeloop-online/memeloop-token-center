#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestTerminalCause {
    GatewayPreAdmission,
    UpstreamHttpStatus,
    ClientDisconnected,
    TransportTimeout,
    TransportOuterDeadline,
    TransportConnectionReset,
    TransportHttp2Reset,
    TransportHttp2GoAway,
    TransportBody,
    TransportDecode,
    TransportRequest,
    TransportOther,
    Http2Reset,
    Http2GoAway,
    ReadTimeout,
    RequestTimeout,
    StreamReadError,
}

impl RequestTerminalCause {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GatewayPreAdmission => "gateway_pre_admission",
            Self::UpstreamHttpStatus => "upstream_http_status",
            Self::ClientDisconnected => "client_disconnected",
            Self::TransportTimeout => "upstream_transport_timeout",
            Self::TransportOuterDeadline => "upstream_transport_outer_deadline",
            Self::TransportConnectionReset => "upstream_transport_connection_reset",
            Self::TransportHttp2Reset => "upstream_transport_http2_reset",
            Self::TransportHttp2GoAway => "upstream_transport_http2_goaway",
            Self::TransportBody => "upstream_transport_body",
            Self::TransportDecode => "upstream_transport_decode",
            Self::TransportRequest => "upstream_transport_request",
            Self::TransportOther => "upstream_transport_other",
            Self::Http2Reset => "upstream_http2_reset",
            Self::Http2GoAway => "upstream_http2_goaway",
            Self::ReadTimeout => "upstream_read_timeout",
            Self::RequestTimeout => "upstream_request_timeout",
            Self::StreamReadError => "upstream_stream_read_error",
        }
    }
}

#[derive(serde::Serialize, Debug, PartialEq, Eq)]
pub struct RequestFailureDiagnostic {
    pub stage: Option<&'static str>,
    pub source: &'static str,
    pub reason: &'static str,
}

pub fn failure_diagnostic(code: Option<&str>) -> Option<RequestFailureDiagnostic> {
    use RequestTerminalCause::*;
    let cause = [
        GatewayPreAdmission,
        UpstreamHttpStatus,
        ClientDisconnected,
        TransportTimeout,
        TransportOuterDeadline,
        TransportConnectionReset,
        TransportHttp2Reset,
        TransportHttp2GoAway,
        TransportBody,
        TransportDecode,
        TransportRequest,
        TransportOther,
        Http2Reset,
        Http2GoAway,
        ReadTimeout,
        RequestTimeout,
        StreamReadError,
    ]
    .into_iter()
    .find(|cause| Some(cause.as_str()) == code)?;
    let (stage, source) = match cause {
        GatewayPreAdmission => (Some("pre_admission"), "gateway"),
        UpstreamHttpStatus => (Some("upstream_response_headers"), "upstream_http_response"),
        ClientDisconnected => (Some("response_delivery"), "downstream_connection"),
        _ => (None, "upstream"),
    };
    Some(RequestFailureDiagnostic {
        stage,
        source,
        reason: cause.as_str(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_require_persisted_allowlisted_evidence() {
        for code in [
            None,
            Some("http_404"),
            Some("http_499"),
            Some("Bearer private-canary"),
        ] {
            assert_eq!(failure_diagnostic(code), None);
        }
        for (cause, stage, source) in [
            (
                RequestTerminalCause::UpstreamHttpStatus,
                Some("upstream_response_headers"),
                "upstream_http_response",
            ),
            (
                RequestTerminalCause::ClientDisconnected,
                Some("response_delivery"),
                "downstream_connection",
            ),
            (RequestTerminalCause::Http2Reset, None, "upstream"),
        ] {
            assert_eq!(
                failure_diagnostic(Some(cause.as_str())),
                Some(RequestFailureDiagnostic {
                    stage,
                    source,
                    reason: cause.as_str(),
                })
            );
        }
    }

    #[test]
    fn a_persisted_transport_reason_does_not_establish_its_observation_phase() {
        for cause in [
            RequestTerminalCause::Http2Reset,
            RequestTerminalCause::TransportHttp2Reset,
            RequestTerminalCause::Http2GoAway,
            RequestTerminalCause::StreamReadError,
            RequestTerminalCause::ReadTimeout,
            RequestTerminalCause::RequestTimeout,
            RequestTerminalCause::TransportDecode,
            RequestTerminalCause::TransportBody,
        ] {
            let diagnostic = failure_diagnostic(Some(cause.as_str())).unwrap();
            assert_eq!(diagnostic.stage, None);
            assert_eq!(diagnostic.source, "upstream");
            assert_eq!(diagnostic.reason, cause.as_str());
            let serialized = serde_json::to_value(diagnostic).unwrap();
            assert!(serialized["stage"].is_null());
        }
    }
}
