use super::*;

pub(in crate::api::proxy) struct PreparedProxyRoute {
    pub(in crate::api::proxy) route: ResolvedUpstream,
    pub(super) forwarded_body: Bytes,
    pub(in crate::api::proxy) upstream_stream: bool,
    pub(in crate::api::proxy) codex_downstream_stream: bool,
    pub(in crate::api::proxy) codex_store_disabled: bool,
    pub(super) codex_session_id: Option<String>,
    pub(in crate::api::proxy) component_request: Option<(PreparedProviderRequest, RequestContext)>,
    pub(super) responses_chat: Option<crate::api::responses_via_chat::Context>,
}

pub(in crate::api::proxy) struct PlannedProxyRoute {
    pub(in crate::api::proxy) route: ResolvedUpstream,
    pub(super) forwarded_json: Value,
    pub(in crate::api::proxy) output_token_ceiling: i64,
    pub(super) upstream_stream: bool,
    pub(super) codex_downstream_stream: bool,
    pub(super) codex_store_disabled: bool,
    pub(super) codex_session_id: Option<String>,
    pub(super) component_context: Option<RequestContext>,
    pub(super) responses_chat: Option<crate::api::responses_via_chat::Context>,
}

impl PlannedProxyRoute {
    pub(in crate::api::proxy) fn is_component(&self) -> bool {
        self.component_context.is_some()
    }

    pub(in crate::api::proxy) fn request_body_ceiling(
        &self,
        original_body_length: usize,
    ) -> Result<usize, AppError> {
        let forwarded_length =
            crate::gateway_body::memory::json_encoded_length(&self.forwarded_json)?;
        Ok(original_body_length.max(forwarded_length))
    }
}

impl PreparedProxyRoute {
    pub(in crate::api::proxy) fn release_request_buffers(&mut self) {
        self.forwarded_body = Bytes::new();
        self.responses_chat = None;
    }

    pub(in crate::api::proxy) fn is_codex(&self) -> bool {
        codex_transport::is_driver(&self.route.driver)
    }
}

#[derive(Clone, Copy)]
pub(in crate::api::proxy) struct ProxyRequestContext<'a> {
    pub(in crate::api::proxy) state: &'a AppState,
    pub(in crate::api::proxy) key: &'a AuthenticatedKey,
    pub(in crate::api::proxy) model: &'a str,
    pub(in crate::api::proxy) protocol: Protocol,
    pub(in crate::api::proxy) request_id: Uuid,
    pub(in crate::api::proxy) request_json: &'a Value,
    /// Computed once after policy rewriting: official Codex client plus an
    /// actual MultiAgentV2 collaboration shape.
    pub(in crate::api::proxy) codex_multi_agent_v2_request: bool,
}

pub(in crate::api::proxy) struct ProxyRoutePlanInput<'a> {
    pub(in crate::api::proxy) request: ProxyRequestContext<'a>,
    pub(in crate::api::proxy) route: ResolvedUpstream,
    pub(in crate::api::proxy) preparation_now: i64,
}

#[derive(Debug, Eq, PartialEq)]
pub(in crate::api::proxy) enum TransportFailureKind {
    Timeout,
    ConnectionReset,
    Body,
    Decode,
    Request,
    Other,
}

impl TransportFailureKind {
    pub(in crate::api::proxy) const fn diagnostic_outcome(&self) -> &'static str {
        match self {
            Self::Timeout => "transport_timeout_delivery_unknown",
            Self::ConnectionReset => "transport_connection_reset_delivery_unknown",
            Self::Body => "transport_body_delivery_unknown",
            Self::Decode => "transport_decode_delivery_unknown",
            Self::Request => "transport_request_delivery_unknown",
            Self::Other => "transport_other_delivery_unknown",
        }
    }

    pub(in crate::api::proxy) const fn error_code(&self) -> &'static str {
        match self {
            Self::Timeout => "upstream_transport_timeout",
            Self::ConnectionReset => "upstream_transport_connection_reset",
            Self::Body => "upstream_transport_body",
            Self::Decode => "upstream_transport_decode",
            Self::Request => "upstream_transport_request",
            Self::Other => "upstream_transport_other",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(in crate::api::proxy) enum ProxySendError {
    RetryableConnection(&'static str),
    RetryableCodexBadRequest,
    CodexBadRequest,
    CandidateUnavailable,
    AmbiguousResponse(&'static str),
    NonRetryableTransport(TransportFailureKind),
    OuterDeadline,
    CredentialUnavailable,
    Credential,
}

pub(in crate::api::proxy) struct ProxyRouteResponse {
    pub(in crate::api::proxy) response: UpstreamResponse,
    pub(in crate::api::proxy) upstream_activity: crate::metrics::ActivityGuard,
    pub(in crate::api::proxy) codex_retry: CodexRetryTerminalGuard,
}
