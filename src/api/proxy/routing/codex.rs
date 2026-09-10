use super::*;
use crate::metrics::CodexBadRequestRetry;

pub(super) mod quota;
#[path = "codex/retry.rs"]
mod retry;

use retry::{AttemptControl, CodexRetryState, observe_bad_request_disposition};
pub(in crate::api::proxy) use retry::{CodexRetryTerminal, CodexRetryTerminalGuard};

const DEFAULT_PRE_DELIVERY_CONNECT_ATTEMPTS: usize = 2;
const MAX_PRE_DELIVERY_CONNECT_ATTEMPTS: usize = 4;
const DEFAULT_PRE_DELIVERY_CONNECT_RETRY_DELAY_MILLIS: u64 = 150;
const MAX_PRE_DELIVERY_CONNECT_RETRY_DELAY_MILLIS: u64 = 2_000;

#[derive(Clone, Copy, Debug)]
pub(in crate::api::proxy) struct CodexRuntimeTransportPolicy {
    pub(in crate::api::proxy) connect_attempts: usize,
    pub(in crate::api::proxy) connect_retry_delay: std::time::Duration,
    pub(in crate::api::proxy) shared_probe_attempts: u32,
    pub(in crate::api::proxy) source: &'static str,
}

#[derive(Clone, Copy)]
struct CodexAttemptContext {
    request_id: Uuid,
    candidate_rank: usize,
    outbound_attempt: usize,
    transport_policy: CodexRuntimeTransportPolicy,
}

pub(in crate::api::proxy) fn runtime_transport_policy(
    config: &Value,
    default_shared_probe_attempts: u32,
) -> CodexRuntimeTransportPolicy {
    let policy = config.get("transport_policy").and_then(Value::as_object);
    let connect_attempts = policy
        .and_then(|policy| policy.get("connect_attempts"))
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| (1..=MAX_PRE_DELIVERY_CONNECT_ATTEMPTS).contains(value))
        .unwrap_or(DEFAULT_PRE_DELIVERY_CONNECT_ATTEMPTS);
    let connect_retry_delay_millis = policy
        .and_then(|policy| policy.get("connect_retry_delay_millis"))
        .and_then(Value::as_u64)
        .filter(|value| *value <= MAX_PRE_DELIVERY_CONNECT_RETRY_DELAY_MILLIS)
        .unwrap_or(DEFAULT_PRE_DELIVERY_CONNECT_RETRY_DELAY_MILLIS);
    let shared_probe_attempts = policy
        .and_then(|policy| policy.get("shared_probe_attempts"))
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value <= crate::config::MAX_UPSTREAM_SHARED_PROBE_ATTEMPTS)
        .unwrap_or(default_shared_probe_attempts);
    CodexRuntimeTransportPolicy {
        connect_attempts,
        connect_retry_delay: std::time::Duration::from_millis(connect_retry_delay_millis),
        shared_probe_attempts,
        source: if policy.is_some() {
            "account_config"
        } else {
            "default"
        },
    }
}

#[cfg(test)]
tokio::task_local! {
    static TEST_PRE_DELIVERY_CONNECT_FAILURES: std::cell::Cell<usize>;
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
) -> Result<ProxyRouteResponse, ProxySendError> {
    let outbound_base_url = codex_transport::outbound_base_url(&route.route.base_url);
    network::validate_codex_transport(
        &outbound_base_url,
        &route.route.config,
        route.route.credential.proxy(),
        state.config.allow_oauth_loopback,
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
    // semantics; connection ambiguity never enters this state machine.
    let mut retry = CodexRetryState::new(route.codex_store_disabled);
    let transport_policy = runtime_transport_policy(
        &route.route.config,
        state.config.upstream_health.shared_probe_attempts,
    );
    loop {
        let (response, upstream_activity) = match send_codex_attempt(
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
            },
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
        let response = UpstreamResponse::Codex(response);
        if response.status() == StatusCode::BAD_REQUEST {
            let disposition = codex_transport::classify_bad_request(response).await;
            observe_bad_request_disposition(&state.metrics, disposition);
            match retry.after_bad_request(disposition) {
                AttemptControl::RetrySameAccount => {
                    // The immutable PreparedProxyRoute preserves the body,
                    // identity, session, and `store: false` contract. This
                    // is the sole same-account replay transition.
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
                response,
                upstream_activity,
                codex_retry: CodexRetryTerminalGuard::new(state.metrics.clone(), retry.outcome()),
            });
        }
        let content_type_class = codex_transport::content_type_class(&response);
        let http_version = codex_transport::http_version_class(&response);
        match codex_transport::admit_event_stream_response(response).await {
            Ok(response) => {
                return Ok(ProxyRouteResponse {
                    response,
                    upstream_activity,
                    codex_retry: CodexRetryTerminalGuard::new(
                        state.metrics.clone(),
                        retry.outcome(),
                    ),
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

async fn send_codex_attempt(
    state: &AppState,
    headers: &HeaderMap,
    target_url: &str,
    route: &PreparedProxyRoute,
    session_id: &str,
    context: CodexAttemptContext,
) -> Result<(wreq::Response, crate::metrics::ActivityGuard), ProxySendError> {
    let CodexAttemptContext {
        request_id,
        candidate_rank,
        outbound_attempt,
        transport_policy,
    } = context;
    for connect_attempt in 1..=transport_policy.connect_attempts {
        match send_codex_attempt_once(state, headers, target_url, route, session_id).await {
            Err(ProxySendError::RetryableConnection(failure_stage))
                if connect_attempt < transport_policy.connect_attempts =>
            {
                tracing::warn!(
                    %request_id,
                    upstream_account_id = %route.route.account_id,
                    candidate_rank,
                    outbound_attempt,
                    connect_attempt,
                    connect_attempt_limit = transport_policy.connect_attempts,
                    transport_policy_source = transport_policy.source,
                    failure_kind = "connection",
                    failure_stage,
                    stage = "codex_pre_delivery_connect_retry",
                    "retrying a Codex connection failure before breaker accounting"
                );
                tokio::time::sleep(transport_policy.connect_retry_delay).await;
            }
            result @ Err(ProxySendError::RetryableConnection(failure_stage)) => {
                tracing::warn!(
                    %request_id,
                    upstream_account_id = %route.route.account_id,
                    candidate_rank,
                    outbound_attempt,
                    connect_attempt,
                    connect_attempt_limit = transport_policy.connect_attempts,
                    transport_policy_source = transport_policy.source,
                    failure_kind = "connection",
                    failure_stage,
                    stage = "codex_pre_delivery_connect_exhausted",
                    "Codex connection retries were exhausted before breaker accounting"
                );
                return result;
            }
            result => return result,
        }
    }
    unreachable!("bounded Codex connection attempt loop always returns")
}

async fn send_codex_attempt_once(
    state: &AppState,
    headers: &HeaderMap,
    target_url: &str,
    route: &PreparedProxyRoute,
    session_id: &str,
) -> Result<(wreq::Response, crate::metrics::ActivityGuard), ProxySendError> {
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
    let mut request = state
        .codex_http
        .post(target_url)
        .body(route.forwarded_body.clone());
    if let Some((proxy_url, _)) = route.route.credential.proxy() {
        let proxy =
            wreq::Proxy::all(proxy_url).map_err(|_| ProxySendError::CandidateUnavailable)?;
        request = request.proxy(proxy);
    }
    let credential_now = credential_application_now();
    let request = codex_transport::apply_wreq_wire_headers(
        request,
        headers,
        &route.route.credential,
        session_id,
        credential_now,
    )
    .map_err(|_| credential_application_error(&route.route.credential, credential_now))?;
    let upstream_activity = state.metrics.active_upstream(&route.route.driver, "proxy");
    let upstream_started = Instant::now();
    let upstream_result = request.send().await;
    state.metrics.observe_upstream(
        &route.route.driver,
        "proxy",
        upstream_result.as_ref().ok().map(wreq::Response::status),
        upstream_started.elapsed(),
    );
    match upstream_result {
        Ok(response) => Ok((response, upstream_activity)),
        Err(error)
            if error.is_connect()
                || error.is_proxy_connect()
                || error.is_dns()
                || error.is_tls() =>
        {
            let stage = if error.is_proxy_connect() {
                "proxy_connect"
            } else if error.is_dns() {
                "dns"
            } else if error.is_tls() {
                "tls"
            } else {
                "connect"
            };
            Err(ProxySendError::RetryableConnection(stage))
        }
        // A send error after the request leaves the client is ambiguous and is
        // never replayed by this state machine.
        Err(_) => Err(ProxySendError::NonRetryableTransport),
    }
}
