#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestTerminalCause {
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
