use ::http::{Version, header};
use ::hyper::body::Body;
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};

use super::*;

#[derive(Clone, Default)]
pub(super) struct BodyConsumption {
    polls: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
}

impl BodyConsumption {
    pub(super) fn attach(&self, request: &mut wreq::Request) {
        if let Some(body) = request.body_mut().take() {
            *request.body_mut() = Some(wreq::Body::wrap(ObservedRequestBody {
                inner: body,
                consumption: self.clone(),
            }));
        }
    }

    pub(super) fn polls(&self) -> u64 {
        self.polls.load(Ordering::Relaxed)
    }

    pub(super) fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

struct ObservedRequestBody {
    inner: wreq::Body,
    consumption: BodyConsumption,
}

impl Body for ObservedRequestBody {
    type Data = bytes::Bytes;
    type Error = wreq::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<::hyper::body::Frame<Self::Data>, Self::Error>>> {
        self.consumption.polls.fetch_add(1, Ordering::Relaxed);
        let result = Pin::new(&mut self.inner).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &result
            && let Some(data) = frame.data_ref()
        {
            self.consumption
                .bytes
                .fetch_add(data.len() as u64, Ordering::Relaxed);
        }
        result
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> ::hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

#[derive(Clone, Copy)]
pub(super) struct TransportIdentity {
    pub(super) client_instance_id: Uuid,
    pub(super) cache_hit: bool,
    pub(super) selected_proxy_matches_client_key: Option<bool>,
    pub(super) connect_attempt: usize,
    pub(super) proxy_member_index: Option<usize>,
    pub(super) proxy_member_count: usize,
    pub(super) selection_epoch: i64,
    pub(super) group_selection_version: Option<i64>,
    pub(super) request_local_selection: bool,
}

pub(super) struct Http2AttemptEvidence {
    request_id: Uuid,
    upstream_account_id: Uuid,
    credential_generation: i64,
    transport_revision: i64,
    candidate_rank: usize,
    outbound_attempt: usize,
    binding_group_id: Option<Uuid>,
    account_proxy: bool,
    identity: TransportIdentity,
    body_consumption: BodyConsumption,
}

impl Http2AttemptEvidence {
    pub(super) fn capture(
        route: &ResolvedUpstream,
        identity: TransportIdentity,
        context: CodexAttemptContext,
        account_proxy: bool,
        body_consumption: BodyConsumption,
    ) -> Self {
        Self {
            request_id: context.request_id,
            upstream_account_id: route.account_id,
            credential_generation: route.credential_generation,
            transport_revision: route.transport_revision,
            candidate_rank: context.candidate_rank,
            outbound_attempt: context.outbound_attempt,
            binding_group_id: route
                .config
                .get(crate::db::transport_proxy_management::CONFIG_KEY)
                .and_then(|value| value.get("group_id"))
                .and_then(Value::as_str)
                .and_then(|value| Uuid::parse_str(value).ok()),
            account_proxy,
            identity,
            body_consumption,
        }
    }

    pub(super) fn observe_error(
        &self,
        error: &(dyn std::error::Error + 'static),
        before_response_headers: bool,
    ) {
        if let Some(http2) = upstream_response::codex_http2_error(error) {
            tracing::warn!(
                request_id = %self.request_id,
                upstream_account_id = %self.upstream_account_id,
                credential_generation = self.credential_generation,
                transport_revision = self.transport_revision,
                candidate_rank = self.candidate_rank,
                outbound_attempt = self.outbound_attempt,
                connect_attempt = self.identity.connect_attempt,
                client_instance_id = %self.identity.client_instance_id,
                client_cache_hit = self.identity.cache_hit,
                selected_proxy_matches_client_key = self.identity.selected_proxy_matches_client_key,
                proxy_member_index = ?self.identity.proxy_member_index,
                proxy_member_count = self.identity.proxy_member_count,
                selection_epoch = self.identity.selection_epoch,
                group_selection_version = ?self.identity.group_selection_version,
                request_local_selection = self.identity.request_local_selection,
                binding_group_id = ?self.binding_group_id,
                account_proxy = self.account_proxy,
                stage = if before_response_headers { "codex_http2_before_response_headers" } else { "codex_http2_response_body" },
                http2_reset = http2.is_reset(),
                http2_goaway = http2.is_go_away(),
                http2_remote = http2.is_remote(),
                http2_library = http2.is_library(),
                http2_reason = ?http2.reason().map(u32::from),
                request_body_polls = self.body_consumption.polls(),
                request_body_consumed_bytes = self.body_consumption.bytes(),
                body_consumption_scope = "transport_body_poll_not_wire_commit",
                client_identity_scope = "cached_client_not_connection_or_stream",
                wire_delivery_state = "unknown",
                "Codex HTTP/2 failure origin evidence"
            );
        }
    }

    pub(super) fn observe_response(self, response: wreq::Response) -> UpstreamResponse {
        use futures_util::StreamExt;
        UpstreamResponse::Prefetched {
            status: response.status(),
            headers: response.headers().clone(),
            version: response.version(),
            content_length: response.content_length(),
            stream: Box::pin(response.bytes_stream().map(move |chunk| {
                chunk.map_err(|error| {
                    self.observe_error(&error, false);
                    upstream_response::codex_http2_failure(&error).map_or(
                        upstream_response::UPSTREAM_STREAM_ERROR,
                        upstream_response::CodexHttp2Failure::error_code,
                    )
                })
            })),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct PreparedRequestEvidence {
    known_payload_bytes: usize,
    body_size_hint_exact: Option<u64>,
    prepared_content_length_count: usize,
    prepared_content_length: Option<u64>,
    content_length_state: &'static str,
    request_protocol_policy: &'static str,
    forbidden_connection_header_count: usize,
    forbidden_connection_header_mask: u8,
    te_present: bool,
    te_trailers_only: bool,
}

impl PreparedRequestEvidence {
    fn capture(request: &wreq::Request, known_payload_bytes: usize) -> Self {
        let headers = request.headers();
        let content_lengths = headers.get_all(header::CONTENT_LENGTH);
        let prepared_content_length_count = content_lengths.iter().count();
        let prepared_content_length = (prepared_content_length_count == 1)
            .then(|| {
                content_lengths
                    .iter()
                    .next()
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
            })
            .flatten();
        let content_length_state = match (prepared_content_length_count, prepared_content_length) {
            (0, _) => "not_set_in_built_request",
            (1, Some(length)) if length == known_payload_bytes as u64 => "explicit_matches_payload",
            (1, Some(_)) => "explicit_mismatch",
            (1, None) => "explicit_invalid",
            _ => "explicit_multiple",
        };
        let forbidden_connection_header_mask = [
            "connection",
            "keep-alive",
            "proxy-connection",
            "transfer-encoding",
            "upgrade",
        ]
        .into_iter()
        .enumerate()
        .fold(0_u8, |mask, (index, name)| {
            mask | (u8::from(headers.contains_key(name)) << index)
        });
        Self {
            known_payload_bytes,
            body_size_hint_exact: request.body().and_then(|body| body.size_hint().exact()),
            prepared_content_length_count,
            prepared_content_length,
            content_length_state,
            request_protocol_policy: match request.version() {
                None => "client_default_negotiation",
                Some(Version::HTTP_10) => "explicit_http_10",
                Some(Version::HTTP_11) => "explicit_http_11",
                Some(Version::HTTP_2) => "explicit_http_2",
                Some(_) => "explicit_other",
            },
            forbidden_connection_header_count: forbidden_connection_header_mask.count_ones()
                as usize,
            forbidden_connection_header_mask,
            te_present: headers.contains_key(header::TE),
            te_trailers_only: headers.get_all(header::TE).iter().all(|value| {
                value.to_str().is_ok_and(|value| {
                    value
                        .split(',')
                        .all(|token| token.trim().eq_ignore_ascii_case("trailers"))
                })
            }),
        }
    }
}

pub(super) fn observe(
    request: &wreq::Request,
    route: &PreparedProxyRoute,
    selected_credential: &UpstreamCredential,
    identity: &TransportIdentity,
    context: CodexAttemptContext,
) {
    let evidence = PreparedRequestEvidence::capture(request, route.forwarded_body.len());
    let binding_group_id = route
        .route
        .config
        .get(crate::db::transport_proxy_management::CONFIG_KEY)
        .and_then(|value| value.get("group_id"))
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok());
    tracing::info!(
        request_id = %context.request_id,
        upstream_account_id = %route.route.account_id,
        credential_generation = route.route.credential_generation,
        transport_revision = route.route.transport_revision,
        candidate_rank = context.candidate_rank,
        outbound_attempt = context.outbound_attempt,
        client_instance_id = %identity.client_instance_id,
        client_cache_hit = identity.cache_hit,
        selected_proxy_matches_client_key = identity.selected_proxy_matches_client_key,
        connect_attempt = identity.connect_attempt,
        configured_outbound_proxy = selected_credential.proxy().is_some(),
        proxy_member_index = ?identity.proxy_member_index,
        proxy_member_count = identity.proxy_member_count,
        selection_epoch = identity.selection_epoch,
        group_selection_version = ?identity.group_selection_version,
        request_local_selection = identity.request_local_selection,
        binding_group_id = ?binding_group_id,
        known_payload_bytes = evidence.known_payload_bytes,
        body_size_hint_exact = ?evidence.body_size_hint_exact,
        prepared_content_length_count = evidence.prepared_content_length_count,
        prepared_content_length = ?evidence.prepared_content_length,
        content_length_state = evidence.content_length_state,
        request_protocol_policy = evidence.request_protocol_policy,
        forbidden_connection_header_count = evidence.forbidden_connection_header_count,
        forbidden_connection_header_mask = evidence.forbidden_connection_header_mask,
        te_present = evidence.te_present,
        te_trailers_only = evidence.te_trailers_only,
        observation_scope = "built_request_before_transport",
        client_identity_scope = "cached_client_not_connection_or_stream",
        body_poll_state = "not_observed",
        wire_delivery_state = "unknown",
        stage = "codex_send_snapshot",
        "Codex non-secret request and selected client snapshot"
    );
}

#[cfg(test)]
#[path = "send_diagnostics_tests.rs"]
mod tests;
