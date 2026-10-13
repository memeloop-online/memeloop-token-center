use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Context, Poll},
};

use futures_util::Stream;

use super::*;
use crate::db::egress::{RequestEgressAttempt, SelectedEgress};

struct Observation {
    database: crate::db::Database,
    queue: Arc<crate::api::proxy::persistence::Persistence>,
    attempt: RequestEgressAttempt,
    request_body: send_diagnostics::BodyConsumption,
    response_started: AtomicBool,
    response_bytes: AtomicU64,
    response_body_complete: AtomicBool,
    completed: AtomicBool,
    final_response: AtomicBool,
    #[cfg(feature = "h2-attempt-binding")]
    binding: Option<wreq_proto::ext::H2AttemptObserver>,
}

impl Observation {
    fn submit(&self) {
        let mut attempt = self.attempt.clone();
        attempt.request_payload_bytes = i64::try_from(self.request_body.bytes()).ok();
        attempt.response_payload_bytes = self
            .response_started
            .load(Ordering::Relaxed)
            .then(|| i64::try_from(self.response_bytes.load(Ordering::Relaxed)).ok())
            .flatten();
        attempt.completed = self.completed.load(Ordering::Relaxed);
        attempt.final_response = self.final_response.load(Ordering::Relaxed);
        attempt.response_body_complete = self.response_body_complete.load(Ordering::Relaxed);
        #[cfg(feature = "h2-attempt-binding")]
        {
            attempt.connection_binding = self
                .binding
                .as_ref()
                .and_then(|observer| observer.snapshot())
                .map(|binding| {
                    serde_json::json!({
                        "connection_id": binding.connection_id,
                        "stream_id": binding.stream_id,
                        "stream_ordinal": binding.stream_ordinal,
                        "scope": "process_local_connection_not_wire_delivery"
                    })
                });
        }
        let database = self.database.clone();
        self.queue.submit(16 * 1024, async move {
            database.record_request_egress_attempt(&attempt).await
        });
    }
}

pub(super) struct AttemptRecorder(Arc<Observation>);

impl AttemptRecorder {
    pub(super) fn begin(
        state: &AppState,
        route: &ResolvedUpstream,
        selected: Option<SelectedEgress>,
        identity: send_diagnostics::TransportIdentity,
        context: CodexAttemptContext,
        request_body: send_diagnostics::BodyConsumption,
        http2_evidence: &send_diagnostics::Http2AttemptEvidence,
    ) -> Option<Self> {
        #[cfg(not(feature = "h2-attempt-binding"))]
        let _ = http2_evidence;
        let Some(selected) = selected else {
            state.routing_persistence.record_failure_gap();
            return None;
        };
        let attempt = RequestEgressAttempt {
            request_id: context.request_id,
            attempt_id: Uuid::now_v7(),
            outbound_attempt: context.outbound_attempt as i64,
            connect_attempt: identity.connect_attempt as i64,
            route_id: route.route_id,
            upstream_account_id: route.account_id,
            credential_generation: route.credential_generation,
            transport_revision: route.transport_revision,
            observed_at: chrono::Utc::now().timestamp_millis(),
            selected,
            transport: serde_json::json!({
                "client_instance_id": identity.client_instance_id,
                "client_cache_hit": identity.cache_hit,
                "selected_proxy_matches_client_key": identity.selected_proxy_matches_client_key,
                "proxy_member_index": identity.proxy_member_index,
                "proxy_member_count": identity.proxy_member_count,
                "selection_epoch": identity.selection_epoch,
                "group_selection_version": identity.group_selection_version,
                "request_local_selection": identity.request_local_selection,
                "candidate_rank": context.candidate_rank,
                "client_identity_scope": "cached_client_not_connection_or_stream",
                "wire_delivery_state": "unknown"
            }),
            connection_binding: None,
            request_payload_bytes: None,
            response_payload_bytes: None,
            completed: false,
            final_response: false,
            response_body_complete: false,
            measurement_basis: "application_payload_estimate",
        };
        crate::db::egress::enqueue_request_egress_attempt(state, attempt.clone());
        Some(Self(Arc::new(Observation {
            database: state.routing_persistence_db.clone(),
            queue: state.routing_persistence.clone(),
            attempt,
            request_body,
            response_started: AtomicBool::new(false),
            response_bytes: AtomicU64::new(0),
            response_body_complete: AtomicBool::new(false),
            completed: AtomicBool::new(false),
            final_response: AtomicBool::new(false),
            #[cfg(feature = "h2-attempt-binding")]
            binding: http2_evidence.binding_observer(),
        })))
    }

    fn finish(&self) {
        if !self.0.completed.swap(true, Ordering::Relaxed) {
            self.0.submit();
        }
    }
}

impl Drop for AttemptRecorder {
    fn drop(&mut self) {
        self.finish();
    }
}

pub(in crate::api::proxy) struct FinalSelection(Arc<Observation>);

impl FinalSelection {
    pub(in crate::api::proxy) fn select(&self) {
        self.0.final_response.store(true, Ordering::Relaxed);
        self.0.submit();
    }
}

pub(super) fn observe_response(
    response: UpstreamResponse,
    recorder: Option<AttemptRecorder>,
) -> (UpstreamResponse, Option<FinalSelection>) {
    let Some(recorder) = recorder else {
        return (response, None);
    };
    recorder.0.response_started.store(true, Ordering::Relaxed);
    let selection = FinalSelection(recorder.0.clone());
    let parts = response.into_parts();
    (
        UpstreamResponse::Prefetched {
            status: parts.status,
            headers: parts.headers,
            version: parts.version,
            content_length: parts.content_length,
            stream: Box::pin(ObservedResponse {
                inner: parts.stream,
                recorder,
            }),
        },
        Some(selection),
    )
}

struct ObservedResponse {
    inner: upstream_response::UpstreamByteStream,
    recorder: AttemptRecorder,
}

impl Stream for ObservedResponse {
    type Item = Result<bytes::Bytes, &'static str>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let result = self.inner.as_mut().poll_next(context);
        match &result {
            Poll::Ready(Some(Ok(chunk))) => {
                self.recorder
                    .0
                    .response_bytes
                    .fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
            Poll::Ready(None) => {
                self.recorder
                    .0
                    .response_body_complete
                    .store(true, Ordering::Relaxed);
                self.recorder.finish();
            }
            Poll::Ready(Some(Err(_))) => self.recorder.finish(),
            Poll::Pending => {}
        }
        result
    }
}
