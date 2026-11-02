use super::*;
use crate::metrics::{CodexBadRequestRetry, CodexEgressFailureStage, CodexEgressPath};

#[path = "codex/egress.rs"]
pub(in crate::api::proxy) mod egress;
pub(super) mod quota;
#[path = "codex/retry.rs"]
mod retry;
#[path = "codex/send_diagnostics.rs"]
mod send_diagnostics;
#[cfg(test)]
#[path = "codex/wire_contract_tests.rs"]
mod wire_contract_tests;

use retry::{AttemptControl, CodexRetryState, observe_bad_request_disposition};
pub(in crate::api::proxy) use retry::{CodexRetryTerminal, CodexRetryTerminalGuard};

#[derive(Clone, Copy, Debug)]
pub(in crate::api::proxy) struct CodexRuntimeTransportPolicy {
    pub(in crate::api::proxy) connect_attempts: usize,
    pub(in crate::api::proxy) shared_probe_attempts: u32,
    pub(in crate::api::proxy) source: &'static str,
    pub(in crate::api::proxy) version: u32,
    pub(in crate::api::proxy) sse_framing_limits: crate::provider::SseFramingLimits,
}

#[derive(Clone, Copy)]
struct CodexAttemptContext {
    request_id: Uuid,
    candidate_rank: usize,
    outbound_attempt: usize,
    transport_policy: CodexRuntimeTransportPolicy,
    deadline: CodexRequestDeadline,
}

#[derive(Clone, Copy)]
struct CodexRequestDeadline {
    request: Option<tokio::time::Instant>,
    read_inactivity: std::time::Duration,
}

#[derive(Clone, Copy)]
enum CandidatePreparationFailure {
    ProxyGroupSelection,
    TransportSnapshot,
}

impl CandidatePreparationFailure {
    fn label(self) -> &'static str {
        match self {
            Self::ProxyGroupSelection => "proxy_group_selection_unavailable",
            Self::TransportSnapshot => "transport_snapshot_unavailable",
        }
    }

    fn map_result<ResultValue, ErrorValue>(
        self,
        result: Result<ResultValue, ErrorValue>,
        route: &ResolvedUpstream,
        context: CodexAttemptContext,
    ) -> Result<ResultValue, ProxySendError> {
        result.map_err(|_| {
            tracing::warn!(
                request_id = %context.request_id,
                route_id = %route.route_id,
                upstream_account_id = %route.account_id,
                credential_generation = route.credential_generation,
                transport_revision = route.transport_revision,
                candidate_rank = context.candidate_rank,
                outbound_attempt = context.outbound_attempt,
                reason = self.label(),
                stage = "codex_candidate_preparation_failed",
                "Codex local candidate preparation failed"
            );
            ProxySendError::CandidateUnavailable
        })
    }
}

pub(in crate::api::proxy) fn runtime_transport_policy(
    config: &Value,
    default_shared_probe_attempts: u32,
) -> Result<CodexRuntimeTransportPolicy, AppError> {
    let policy = crate::provider::CodexTransportPolicy::parse(config.get("transport_policy"))
        .map_err(|_| AppError::BadRequest("invalid Codex transport policy".into()))?;
    Ok(CodexRuntimeTransportPolicy {
        connect_attempts: policy.connect_attempts,
        shared_probe_attempts: policy
            .shared_probe_attempts
            .unwrap_or(default_shared_probe_attempts),
        version: policy.version,
        source: if config.get("transport_policy").is_some() {
            "account_config"
        } else {
            "global_default"
        },
        sse_framing_limits: policy.sse_framing_limits(),
    })
}

#[cfg(test)]
tokio::task_local! {
    static TEST_PRE_DELIVERY_CONNECT_FAILURES: std::cell::Cell<usize>;
    static TEST_SEND_FAILURES: std::cell::RefCell<std::collections::VecDeque<ProxySendError>>;
}

#[cfg(test)]
pub(in crate::api::proxy) async fn with_test_send_failures<F: std::future::Future>(
    failures: Vec<ProxySendError>,
    future: F,
) -> (F::Output, usize) {
    TEST_SEND_FAILURES
        .scope(std::cell::RefCell::new(failures.into()), async {
            let output = future.await;
            let remaining = TEST_SEND_FAILURES.with(|failures| failures.borrow().len());
            (output, remaining)
        })
        .await
}

#[cfg(test)]
pub(in crate::api::proxy) async fn with_test_pre_delivery_connect_failures<F>(
    failures: usize,
    future: F,
) -> F::Output
where
    F: std::future::Future,
{
    TEST_PRE_DELIVERY_CONNECT_FAILURES
        .scope(std::cell::Cell::new(failures), future)
        .await
}

pub(super) fn validate_route(route: &ResolvedUpstream, protocol: Protocol) -> Result<(), AppError> {
    codex_transport::validate_protocol(protocol)?;
    if route.base_url != codex_transport::BASE_URL {
        return Err(AppError::BadRequest(
            "OpenAI Codex account has an invalid fixed base URL".into(),
        ));
    }
    codex_transport::validate_credential_contract(&route.credential)?;
    codex_transport::validate_route_config(&route.config)
}

pub(super) async fn send_proxy_route(
    state: &AppState,
    headers: &HeaderMap,
    request_id: Uuid,
    route: &PreparedProxyRoute,
    candidate_rank: usize,
    outbound_attempt: usize,
    h2_recovery_available: &mut bool,
) -> Result<ProxyRouteResponse, ProxySendError> {
    let outbound_base_url = codex_transport::outbound_base_url(&route.route.base_url);
    network::validate_codex_transport(
        &outbound_base_url,
        &route.route.config,
        route.route.credential.proxy(),
        state.config.codex_test_loopback,
    )
    .await
    .map_err(|_| ProxySendError::CandidateUnavailable)?;
    let target_url = network::upstream_api_url(&outbound_base_url, codex_transport::RESPONSES_PATH);
    let session_id = route
        .codex_session_id
        .as_deref()
        .ok_or(ProxySendError::Credential)?;
    // `prepare_request_with_id` has forced the exact outbound document to be
    // non-persistent. An HTTP 400 is still replayable only after a complete,
    // bounded, domain-level classification identifies known transient
    // semantics.
    let mut retry = CodexRetryState::new(route.codex_store_disabled);
    let transport_policy = runtime_transport_policy(
        &route.route.config,
        state.config.upstream_health.shared_probe_attempts,
    )
    .map_err(|_| ProxySendError::CandidateUnavailable)?;
    tracing::info!(
        %request_id,
        upstream_account_id = %route.route.account_id,
        transport_policy_source = transport_policy.source,
        transport_policy_version = transport_policy.version,
        max_sse_event_bytes = transport_policy.sse_framing_limits.event_bytes,
        max_sse_framed_bytes = transport_policy.sse_framing_limits.framed_bytes,
        max_sse_terminal_hold_bytes = transport_policy.sse_framing_limits.terminal_hold_bytes,
        stage = "sse_framing_policy_snapshot",
        "upstream SSE framing policy frozen"
    );
    // Compaction and ordinary generation share one rule: no gateway clock.
    // The attempt ends when the upstream answers, the client cancels, or the
    // socket fails. `wait_for_compaction` remains part of the route record.
    let _ = route.wait_for_compaction;
    let deadline = CodexRequestDeadline {
        request: None,
        read_inactivity: std::time::Duration::ZERO,
    };
    loop {
        let (response, upstream_activity, egress_selection) = match send_codex_attempt(
            state,
            headers,
            &target_url,
            route,
            session_id,
            CodexAttemptContext {
                request_id,
                candidate_rank,
                outbound_attempt,
                transport_policy,
                deadline,
            },
            h2_recovery_available,
        )
        .await
        {
            Ok(response) => response,
            Err(error) => {
                retry
                    .outcome()
                    .observe_terminal(&state.metrics, CodexRetryTerminal::Failed);
                return Err(error);
            }
        };
        if response.status() == StatusCode::BAD_REQUEST {
            let disposition = codex_transport::classify_bad_request(response, request_id).await;
            observe_bad_request_disposition(&state.metrics, disposition);
            match retry.after_bad_request(disposition) {
                AttemptControl::RetrySameAccount => {
                    // The immutable PreparedProxyRoute preserves the body,
                    // identity, session, and `store: false` contract.
                    state
                        .metrics
                        .observe_codex_bad_request_retry(CodexBadRequestRetry::Started);
                    drop(upstream_activity);
                    continue;
                }
                AttemptControl::Return(error) => {
                    retry
                        .outcome()
                        .observe_terminal(&state.metrics, CodexRetryTerminal::Failed);
                    return Err(error);
                }
            }
        }
        if !response.status().is_success() {
            return Ok(ProxyRouteResponse {
                egress_selection,
                response,
                upstream_activity,
                codex_retry: CodexRetryTerminalGuard::new(state.metrics.clone(), retry.outcome()),
                sse_framing_limits: transport_policy.sse_framing_limits,
            });
        }
        let content_type_class = codex_transport::content_type_class(&response);
        let http_version = codex_transport::http_version_class(&response);
        match codex_transport::admit_event_stream_response(
            response,
            transport_policy.sse_framing_limits,
        )
        .await
        {
            Ok(response) => {
                return Ok(ProxyRouteResponse {
                    egress_selection,
                    response,
                    upstream_activity,
                    codex_retry: CodexRetryTerminalGuard::new(
                        state.metrics.clone(),
                        retry.outcome(),
                    ),
                    sse_framing_limits: transport_policy.sse_framing_limits,
                });
            }
            Err(codex_transport::ResponseAdmissionError::Invalid(error_code)) => {
                retry
                    .outcome()
                    .observe_terminal(&state.metrics, CodexRetryTerminal::Failed);
                tracing::warn!(
                    %request_id,
                    upstream_account_id = %route.route.account_id,
                    content_type_class,
                    http_version,
                    stage = error_code,
                    "Codex upstream response failed framing admission"
                );
                // A successful HTTP response means the POST may already have
                // executed and become billable. Framing invalidity is safe to
                // reject, but never safe to replay on another account.
                return Err(ProxySendError::AmbiguousResponse(error_code));
            }
            Err(codex_transport::ResponseAdmissionError::Ambiguous(error_code)) => {
                retry
                    .outcome()
                    .observe_terminal(&state.metrics, CodexRetryTerminal::Failed);
                tracing::warn!(
                    %request_id,
                    upstream_account_id = %route.route.account_id,
                    content_type_class,
                    http_version,
                    stage = error_code,
                    "Codex upstream response failed before framing admission completed"
                );
                return Err(ProxySendError::AmbiguousResponse(error_code));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_codex_attempt(
    state: &AppState,
    headers: &HeaderMap,
    target_url: &str,
    route: &PreparedProxyRoute,
    session_id: &str,
    context: CodexAttemptContext,
    h2_recovery_available: &mut bool,
) -> Result<
    (
        UpstreamResponse,
        crate::metrics::ActivityGuard,
        Option<egress::FinalSelection>,
    ),
    ProxySendError,
> {
    let CodexAttemptContext {
        request_id,
        candidate_rank,
        outbound_attempt,
        transport_policy,
        deadline: _,
    } = context;
    let selection = CandidatePreparationFailure::ProxyGroupSelection.map_result(
        state.transport_proxy_groups.select_config(
            route.route.account_id,
            route.route.credential_generation,
            &route.route.credential,
            &route.route.config,
        ),
        &route.route,
        context,
    )?;
    tracing::debug!(
        %request_id,
        upstream_account_id = %route.route.account_id,
        transport_revision = route.route.transport_revision,
        credential_generation = route.route.credential_generation,
        candidate_rank,
        outbound_attempt,
        member_index = ?selection.member_index(),
        member_count = selection.member_count(),
        selection_epoch = selection.generation,
        group_selection_version = ?selection.group_selection_version(),
        request_local = selection.is_request_local(),
        configured_connect_attempts = transport_policy.connect_attempts,
        stage = "codex_proxy_member_selection",
        "Codex request uses the sticky proxy exit"
    );
    let mut transport_route = route.route.clone();
    transport_route.credential = selection.credential.clone();
    let client_snapshot = CandidatePreparationFailure::TransportSnapshot.map_result(
        state
            .codex_clients
            .transport_snapshot_with_diagnostics(&transport_route, selection.generation),
        &route.route,
        context,
    )?;
    let transport_identity = send_diagnostics::TransportIdentity {
        client_instance_id: client_snapshot.instance_id,
        cache_hit: client_snapshot.cache_hit,
        selected_proxy_matches_client_key: client_snapshot
            .matches_selected_proxy(&selection.credential),
        connect_attempt: 1,
        proxy_member_index: selection.member_index(),
        proxy_member_count: selection.member_count(),
        selection_epoch: selection.generation,
        group_selection_version: selection.group_selection_version(),
        request_local_selection: selection.is_request_local(),
    };
    let phase = proxy_diagnostics::Phase::account(
        proxy_diagnostics::Context::for_request(request_id),
        "codex_transport_attempt",
        Some(route.route.account_id),
        Some(route.route.credential_generation),
    );
    let result = send_codex_attempt_once(
        state,
        headers,
        target_url,
        route,
        session_id,
        (
            &client_snapshot.client,
            &selection.credential,
            transport_identity,
            selection.egress_snapshot(),
        ),
        context,
        h2_recovery_available,
    )
    .await;
    phase.finish(
        match &result {
            Ok(_) => "response_headers",
            Err(ProxySendError::RetryableConnection(_)) => "connect_not_delivered",
            Err(_) => "failed",
        },
        result
            .as_ref()
            .ok()
            .map(|(response, ..)| response.status().as_u16()),
        None,
    );
    if let Err(ProxySendError::RetryableConnection(failure_stage)) = &result {
        selection.request_independent_probe();
        tracing::warn!(
            %request_id,
            upstream_account_id = %route.route.account_id,
            candidate_rank,
            outbound_attempt,
            member_index = ?selection.member_index(),
            failure_stage,
            stage = "codex_proxy_probe_requested",
            "Codex connection failure keeps the sticky exit and requests an independent probe"
        );
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn send_codex_attempt_once(
    state: &AppState,
    headers: &HeaderMap,
    target_url: &str,
    route: &PreparedProxyRoute,
    session_id: &str,
    transport: (
        &reqwest::Client,
        &UpstreamCredential,
        send_diagnostics::TransportIdentity,
        Option<crate::db::egress::SelectedEgress>,
    ),
    context: CodexAttemptContext,
    h2_recovery_available: &mut bool,
) -> Result<
    (
        UpstreamResponse,
        crate::metrics::ActivityGuard,
        Option<egress::FinalSelection>,
    ),
    ProxySendError,
> {
    let CodexAttemptContext {
        request_id,
        deadline,
        ..
    } = context;
    #[cfg(test)]
    if let Ok(error) = TEST_SEND_FAILURES.try_with(|failures| {
        failures
            .borrow_mut()
            .pop_front()
            .expect("unexpected additional upstream send")
    }) {
        return Err(error);
    }
    #[cfg(test)]
    if TEST_PRE_DELIVERY_CONNECT_FAILURES
        .try_with(|remaining| {
            let current = remaining.get();
            remaining.set(current.saturating_sub(1));
            current > 0
        })
        .unwrap_or(false)
    {
        return Err(ProxySendError::RetryableConnection("test_injected"));
    }
    let (client, credential, transport_identity, selected_egress) = transport;
    // Plugins do not replace this send unless one registers. None do: a package
    // that needs its own HTTP calls host `http-request`, executed by this client.
    if state.plugins.codex_outbound_registered() {
        return Err(ProxySendError::CandidateUnavailable);
    }
    loop {
        let request = client.post(target_url).body(route.forwarded_body.clone());
        let egress_path = if credential.proxy().is_some() {
            CodexEgressPath::AccountProxy
        } else {
            CodexEgressPath::Direct
        };
        let credential_now = credential_application_now();
        let request = codex_transport::apply_codex_wire_headers(
            request,
            headers,
            &route.route.credential,
            session_id,
            credential_now,
        )
        .map_err(|_| credential_application_error(&route.route.credential, credential_now))?;
        let (client, request) = request.build_split();
        if let Ok(request) = &request {
            send_diagnostics::observe(request, route, credential, &transport_identity, context);
        }
        let upstream_activity = state.metrics.active_upstream(&route.route.driver, "proxy");
        let upstream_started = Instant::now();
        let body_consumption = send_diagnostics::BodyConsumption::default();
        let http2_evidence = send_diagnostics::Http2AttemptEvidence::capture(
            &route.route,
            transport_identity,
            context,
            matches!(egress_path, CodexEgressPath::AccountProxy),
            body_consumption.clone(),
        );
        let mut egress = None;
        let mut remote_reset = false;
        let upstream_result = send_until_request_deadline(deadline.request, async {
        async {
            let mut request = request?;
            body_consumption.attach(&mut request);
            egress = egress::AttemptRecorder::begin(
                state,
                &route.route,
                selected_egress.clone(),
                transport_identity,
                context,
                body_consumption.clone(),
                &http2_evidence,
            );
            IndependentProbeTarget::dispatched(&route.route);
            let response = client.execute(request).await?;
            IndependentProbeTarget::response(response.status());
            Ok::<_, reqwest::Error>(response)
        }
        .await
        .map_err(|error| {
            remote_reset = upstream_response::codex_http2_error(&error)
                .is_some_and(|error| error.is_reset() && error.is_remote() && !error.is_library());
            http2_evidence.observe_error(&error, true);
            tracing::warn!(
                %request_id,
                upstream_account_id = %route.route.account_id,
                configured_outbound_proxy = matches!(egress_path, CodexEgressPath::AccountProxy),
                transport_revision = route.route.transport_revision,
                credential_generation = route.route.credential_generation,
                candidate_rank = context.candidate_rank,
                outbound_attempt = context.outbound_attempt,
                connect_attempt = transport_identity.connect_attempt,
                client_instance_id = %transport_identity.client_instance_id,
                client_cache_hit = transport_identity.cache_hit,
                selected_proxy_matches_client_key = transport_identity.selected_proxy_matches_client_key,
                proxy_member_index = ?transport_identity.proxy_member_index,
                selection_epoch = transport_identity.selection_epoch,
                group_selection_version = ?transport_identity.group_selection_version,
                request_local_selection = transport_identity.request_local_selection,
                transport_cause = codex_transport_cause(&error),
                request_body_polls = body_consumption.polls(),
                request_body_consumed_bytes = body_consumption.bytes(),
                body_consumption_scope = "transport_body_poll_not_wire_commit",
                wire_delivery_state = "unknown",
                send_elapsed_ms = upstream_started.elapsed().as_millis(),
                stage = "codex_transport_send_failure",
                "Codex outbound transport send failed"
            );
            classify_codex_send_error(
                error,
                body_consumption.polls(),
                body_consumption.bytes(),
            )
        })
    })
    .await;
        record_codex_send_attempt_failure(&state.metrics, egress_path, &upstream_result);
        state.metrics.observe_upstream(
            &route.route.driver,
            "proxy",
            upstream_result.as_ref().ok().map(reqwest::Response::status),
            upstream_started.elapsed(),
        );
        match upstream_result {
            Ok(response) => {
                let response = http2_evidence.observe_response(response);
                let (response, egress_selection) = egress::observe_response(response, egress);
                let response = match deadline.request {
                    Some(request_deadline) => {
                        response.with_body_timeouts(request_deadline, deadline.read_inactivity)
                    }
                    None => response,
                };
                return Ok((response, upstream_activity, egress_selection));
            }
            Err(_) if remote_reset && *h2_recovery_available => {
                *h2_recovery_available = false;
                tracing::warn!(
                    %request_id,
                    upstream_account_id = %route.route.account_id,
                    candidate_rank = context.candidate_rank,
                    outbound_attempt = context.outbound_attempt,
                    recovery_attempt = 1,
                    recovery_attempt_limit = 1,
                    recovery_phase = "before_response_headers",
                    accounting_scope = "original_logical_request_reservation",
                    recovery_policy_source = "codex_native_pre_header_remote_reset_v1",
                    transport_policy_source = context.transport_policy.source,
                    transport_policy_version = context.transport_policy.version,
                    failure_code = "upstream_transport_http2_reset",
                    stage = "codex_h2_server_recovery",
                    "recovering a remote HTTP/2 reset before response headers"
                );
                drop(egress);
                drop(upstream_activity);
                tokio::task::yield_now().await;
            }
            Err(error) => return Err(error),
        }
    }
}

fn classify_codex_send_error(
    error: reqwest::Error,
    body_polls: u64,
    body_bytes: u64,
) -> ProxySendError {
    if let Some(http2_failure) = upstream_response::codex_http2_failure(&error) {
        let kind = match http2_failure {
            upstream_response::CodexHttp2Failure::Reset => TransportFailureKind::Http2Reset,
            upstream_response::CodexHttp2Failure::GoAway => TransportFailureKind::Http2GoAway,
        };
        ProxySendError::NonRetryableTransport(kind)
    } else {
        let class = crate::simple_http::classify_transport(&error);
        if class.pre_delivery() {
            return ProxySendError::RetryableConnection(class.stage());
        }
        // reqwest surfaces a `connect_timeout` expiry as a plain request timeout,
        // not as a connect error, so `pre_delivery()` cannot see it. With zero
        // body polls the request body never reached the wire, so the logical
        // request was provably not delivered; replaying it on another candidate
        // (or another sticky exit) cannot double-charge the upstream.
        if class.timeout && body_polls == 0 && body_bytes == 0 {
            return ProxySendError::RetryableConnection("connect_timeout");
        }
        // A send error after the request leaves the client is ambiguous and is
        // never replayed by this state machine.
        let kind = if class.timeout || error.is_timeout() {
            TransportFailureKind::Timeout
        } else if class.connection_reset {
            TransportFailureKind::ConnectionReset
        } else if error.is_body() {
            TransportFailureKind::Body
        } else if error.is_decode() {
            TransportFailureKind::Decode
        } else if error.is_request() {
            TransportFailureKind::Request
        } else {
            TransportFailureKind::Other
        };
        ProxySendError::NonRetryableTransport(kind)
    }
}

/// Report the real transport cause. These messages describe a socket, TLS or
/// HTTP failure against an upstream and carry no credential material, so the
/// full source chain is logged instead of a masked label. Masking it hid the
/// reason requests failed and made upstream faults undiagnosable.
fn codex_transport_cause(error: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = Vec::new();
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    for _ in 0..16 {
        let current = match source {
            Some(current) => current,
            None => break,
        };
        parts.push(current.to_string());
        source = current.source();
    }
    parts.join(" <- ")
}

fn codex_egress_failure_stage(error: &ProxySendError) -> CodexEgressFailureStage {
    match error {
        ProxySendError::RetryableConnection("proxy_connect") => {
            CodexEgressFailureStage::ProxyConnect
        }
        ProxySendError::RetryableConnection("dns") => CodexEgressFailureStage::Dns,
        ProxySendError::RetryableConnection("tls") => CodexEgressFailureStage::Tls,
        ProxySendError::RetryableConnection("connect") => CodexEgressFailureStage::Connect,
        ProxySendError::RetryableConnection("connect_timeout") => CodexEgressFailureStage::Connect,
        ProxySendError::NonRetryableTransport(TransportFailureKind::Timeout) => {
            CodexEgressFailureStage::Timeout
        }
        ProxySendError::NonRetryableTransport(TransportFailureKind::ConnectionReset) => {
            CodexEgressFailureStage::ConnectionReset
        }
        ProxySendError::NonRetryableTransport(TransportFailureKind::Http2Reset) => {
            CodexEgressFailureStage::Http2Reset
        }
        ProxySendError::NonRetryableTransport(TransportFailureKind::Http2GoAway) => {
            CodexEgressFailureStage::Http2GoAway
        }
        ProxySendError::NonRetryableTransport(TransportFailureKind::Body) => {
            CodexEgressFailureStage::Body
        }
        ProxySendError::NonRetryableTransport(TransportFailureKind::Decode) => {
            CodexEgressFailureStage::Decode
        }
        ProxySendError::NonRetryableTransport(TransportFailureKind::Request) => {
            CodexEgressFailureStage::Request
        }
        ProxySendError::AmbiguousResponse(upstream_response::UPSTREAM_REQUEST_TIMEOUT) => {
            CodexEgressFailureStage::Timeout
        }
        _ => CodexEgressFailureStage::Other,
    }
}

fn record_codex_send_attempt_failure<T>(
    metrics: &crate::metrics::Metrics,
    path: CodexEgressPath,
    result: &Result<T, ProxySendError>,
) {
    if let Err(error) = result {
        metrics.record_codex_egress_send_failure(path, codex_egress_failure_stage(error));
    }
}

async fn send_until_request_deadline<T, F>(
    deadline: impl Into<Option<tokio::time::Instant>>,
    send: F,
) -> Result<T, ProxySendError>
where
    F: std::future::Future<Output = Result<T, ProxySendError>>,
{
    let Some(deadline) = deadline.into() else {
        return send.await;
    };
    tokio::pin!(send);
    tokio::select! {
        // This budget is absolute. A result that becomes observable only at
        // or after the deadline cannot authorize delivery or replay.
        biased;
        _ = tokio::time::sleep_until(deadline) => Err(ProxySendError::AmbiguousResponse(
            super::super::upstream_response::UPSTREAM_REQUEST_TIMEOUT,
        )),
        result = &mut send => result,
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::super::outcome::FailoverDisposition;
    use super::*;
    use serde_json::json;

    #[derive(Debug)]
    struct TestError {
        label: &'static str,
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    }

    impl std::fmt::Display for TestError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(self.label)
        }
    }

    impl std::error::Error for TestError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.source.as_deref().map(|error| error as _)
        }
    }

    #[test]
    fn transport_cause_reports_the_real_source_chain() {
        let keepalive = TestError {
            label: "keep-alive timed out",
            source: Some(Box::new(TestError {
                label: "operation timed out",
                source: None,
            })),
        };
        assert_eq!(
            codex_transport_cause(&keepalive),
            "keep-alive timed out <- operation timed out"
        );
        let read_timeout = std::io::Error::new(std::io::ErrorKind::TimedOut, "private endpoint");
        assert_eq!(codex_transport_cause(&read_timeout), "private endpoint");
    }

    #[test]
    fn transport_cause_scan_is_bounded() {
        let mut error: Box<dyn std::error::Error + Send + Sync> = Box::new(TestError {
            label: "root",
            source: None,
        });
        for _ in 0..32 {
            error = Box::new(TestError {
                label: "wrapper",
                source: Some(error),
            });
        }
        // The walk stops after a bounded number of links instead of walking an
        // arbitrarily deep chain.
        assert_eq!(
            codex_transport_cause(error.as_ref())
                .matches("wrapper")
                .count(),
            16
        );
    }

    #[test]
    fn send_failure_stage_projects_existing_classification_without_replay_changes() {
        for (error, stage) in [
            (
                ProxySendError::RetryableConnection("proxy_connect"),
                CodexEgressFailureStage::ProxyConnect,
            ),
            (
                ProxySendError::RetryableConnection("dns"),
                CodexEgressFailureStage::Dns,
            ),
            (
                ProxySendError::RetryableConnection("tls"),
                CodexEgressFailureStage::Tls,
            ),
            (
                ProxySendError::RetryableConnection("connect"),
                CodexEgressFailureStage::Connect,
            ),
            (
                ProxySendError::NonRetryableTransport(TransportFailureKind::Timeout),
                CodexEgressFailureStage::Timeout,
            ),
            (
                ProxySendError::NonRetryableTransport(TransportFailureKind::ConnectionReset),
                CodexEgressFailureStage::ConnectionReset,
            ),
            (
                ProxySendError::NonRetryableTransport(TransportFailureKind::Body),
                CodexEgressFailureStage::Body,
            ),
            (
                ProxySendError::NonRetryableTransport(TransportFailureKind::Decode),
                CodexEgressFailureStage::Decode,
            ),
            (
                ProxySendError::NonRetryableTransport(TransportFailureKind::Request),
                CodexEgressFailureStage::Request,
            ),
            (
                ProxySendError::NonRetryableTransport(TransportFailureKind::Http2Reset),
                CodexEgressFailureStage::Http2Reset,
            ),
            (
                ProxySendError::NonRetryableTransport(TransportFailureKind::Http2GoAway),
                CodexEgressFailureStage::Http2GoAway,
            ),
            (
                ProxySendError::AmbiguousResponse(
                    super::super::upstream_response::UPSTREAM_REQUEST_TIMEOUT,
                ),
                CodexEgressFailureStage::Timeout,
            ),
            (
                ProxySendError::AmbiguousResponse("response_admission_failed"),
                CodexEgressFailureStage::Other,
            ),
            (
                ProxySendError::NonRetryableTransport(TransportFailureKind::Other),
                CodexEgressFailureStage::Other,
            ),
            (
                ProxySendError::RetryableConnection("test_injected"),
                CodexEgressFailureStage::Other,
            ),
        ] {
            assert_eq!(codex_egress_failure_stage(&error), stage);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn absolute_send_deadline_counts_once_without_changing_disposition() {
        let metrics = crate::metrics::Metrics::default();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        let result = send_until_request_deadline(
            deadline,
            std::future::pending::<Result<(), ProxySendError>>(),
        )
        .await;
        record_codex_send_attempt_failure(&metrics, CodexEgressPath::AccountProxy, &result);

        assert!(matches!(
            result,
            Err(ProxySendError::AmbiguousResponse(
                super::super::upstream_response::UPSTREAM_REQUEST_TIMEOUT
            ))
        ));
        assert_eq!(
            failover_disposition(None, result.as_ref().err()),
            FailoverDisposition::Stop
        );
        let rendered = metrics.render(&crate::metrics::RuntimeMetrics::default());
        assert!(rendered.contains(
            "memeloop_token_center_codex_egress_send_failures_total{path=\"account_proxy\",stage=\"timeout\"} 1"
        ));
        assert!(rendered.contains(
            "memeloop_token_center_codex_egress_send_failures_total{path=\"direct\",stage=\"timeout\"} 0"
        ));
    }

    #[test]
    fn runtime_policy_snapshots_current_sse_limits_per_request() {
        let defaults = runtime_transport_policy(&json!({}), 1).unwrap();
        assert_eq!(defaults.source, "global_default");
        assert_eq!(
            defaults.sse_framing_limits,
            crate::provider::SseFramingLimits::default()
        );
        let first = runtime_transport_policy(
            &json!({"transport_policy": {
                "max_sse_event_bytes": 1048576,
                "max_sse_framed_bytes": 1114112,
                "max_sse_terminal_hold_bytes": 1114112
            }}),
            1,
        )
        .unwrap();
        assert_eq!(first.source, "account_config");
        let reloaded = runtime_transport_policy(
            &json!({"transport_policy": {
                "max_sse_event_bytes": 2097152,
                "max_sse_framed_bytes": 2162688,
                "max_sse_terminal_hold_bytes": 2162688
            }}),
            1,
        )
        .unwrap();
        assert_eq!(first.sse_framing_limits.event_bytes, 1_048_576);
        assert_eq!(reloaded.sse_framing_limits.event_bytes, 2_097_152);
        assert_ne!(first.sse_framing_limits, reloaded.sse_framing_limits);
    }

    #[tokio::test(start_paused = true)]
    async fn request_deadline_wins_at_the_typed_connect_boundary() {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        let result = send_until_request_deadline(deadline, async move {
            tokio::time::sleep_until(deadline).await;
            Err::<(), _>(ProxySendError::RetryableConnection("proxy_connect"))
        })
        .await;
        assert!(matches!(
            result,
            Err(ProxySendError::AmbiguousResponse(
                super::super::upstream_response::UPSTREAM_REQUEST_TIMEOUT
            ))
        ));
        assert_eq!(
            failover_disposition(None, result.as_ref().err()),
            FailoverDisposition::Stop
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ready_send_result_cannot_cross_an_expired_request_deadline() {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        tokio::time::advance(std::time::Duration::from_secs(1)).await;

        let result =
            send_until_request_deadline(deadline, async { Ok::<_, ProxySendError>(()) }).await;

        assert!(matches!(
            result,
            Err(ProxySendError::AmbiguousResponse(
                super::super::upstream_response::UPSTREAM_REQUEST_TIMEOUT
            ))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn unclassified_header_deadline_never_grants_replay() {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        let result = send_until_request_deadline(
            deadline,
            std::future::pending::<Result<(), ProxySendError>>(),
        )
        .await;
        assert!(matches!(
            result,
            Err(ProxySendError::AmbiguousResponse(
                super::super::upstream_response::UPSTREAM_REQUEST_TIMEOUT
            ))
        ));
        assert_eq!(
            failover_disposition(None, result.as_ref().err()),
            FailoverDisposition::Stop
        );
    }

    #[tokio::test(start_paused = true)]
    async fn total_request_deadline_is_shared_across_the_permitted_replay() {
        let started = tokio::time::Instant::now();
        let deadline = started + std::time::Duration::from_secs(1);
        let first = send_until_request_deadline(deadline, async {
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            Ok::<_, ProxySendError>(())
        })
        .await;
        assert!(first.is_ok());

        let replay = send_until_request_deadline(
            deadline,
            std::future::pending::<Result<(), ProxySendError>>(),
        )
        .await;
        assert!(matches!(
            replay,
            Err(ProxySendError::AmbiguousResponse(
                super::super::upstream_response::UPSTREAM_REQUEST_TIMEOUT
            ))
        ));
        assert_eq!(
            tokio::time::Instant::now() - started,
            std::time::Duration::from_secs(1)
        );
    }
}
