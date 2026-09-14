use super::*;
pub(crate) use routing::{RequestAttemptBudget as MediaAttemptBudget, wait_media_recovery};
pub(crate) use routing::{
    UpstreamAttemptGuard as MediaAttemptGuard, UpstreamAttemptTerminal as MediaAttemptTerminal,
};

pub(crate) async fn classify_media_rate_limit(
    response: reqwest::Response,
) -> crate::db::UpstreamFailureKind {
    let (response, kind) = routing::classify_rate_limit(response.into()).await;
    drop(response);
    kind
}

#[path = "codex_transport.rs"]
pub(in crate::api) mod codex_transport;

mod buffered_upstream;
mod chat_sse_usage;
mod conversation_hints;
mod lifecycle;
mod response_metadata;
mod routing;
mod sse_capture;
mod streaming;
mod upstream_response;

#[cfg(test)]
use crate::db::UpstreamFailureKind;
use crate::response_archive_spool::BufferedArchive;
use crate::{
    db::{ProxyRequestUpstreamAttribution, SwitchProxyCandidateInput, UpstreamAttemptAdmission},
    metrics::{UpstreamHealthEvent, UpstreamHealthReason},
    provider::AuthorizedUpstreamCandidate,
};
use buffered_upstream::read_bounded_upstream;
use chat_sse_usage::ChatSseUsageContract;
pub(in crate::api) use conversation_hints::safe_conversation_hint as safe_response_id;
use conversation_hints::{client_name, conversation_hints};
use lifecycle::{
    finish_proxy_request_with_archive_fallback, finish_unavailable, run_bounded_proxy_lifecycle,
};
use routing::{
    AdmittedProxyRouteInput, CandidatePreparationSummary, CodexRetryTerminal,
    CodexRetryTerminalGuard, DeferredSharedProbe, NextSendableProxyRouteInput, PlannedProxyRoute,
    PreparedProxyRoute, PreparedRouteReadiness, ProxyRequestContext, ProxyRoutePlanInput,
    ProxySendError, UpstreamAttemptGuard, UpstreamAttemptTerminal, candidate_reservation_bounds,
    exhausted_candidate_error, materialize_proxy_route, next_planned_proxy_candidate,
    plan_proxy_route, prepare_admitted_proxy_route, prepared_input_reservation_bound,
    refresh_route_snapshot, retain_pinned_text_candidates, send_proxy_route,
};
use upstream_response::UpstreamResponse;

use response_metadata::{
    ExtractedUsage, append_bounded, extract_response_id, extract_usage_checked,
    is_supported_service_tier, should_capture_buffered_usage,
};
#[cfg(test)]
use response_metadata::{completed_response_id, usage_from_value, usage_from_value_checked};
use sse_capture::{
    ResponsesSseCapture, ResponsesSseOutcome, ResponsesSseSummary, SseDeliveryFrame,
};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod sse_delivery_tests;

const PROXY_BODY_CHANNEL_CAPACITY: usize = 1;
const MAX_INPUT_TOKEN_OVERHEAD_CEILING: i64 = 1_000_000;
const RETAINED_REQUEST_ADMISSION_WAIT: Duration = Duration::from_secs(1);

fn validate_openai_chat_choice_count(request: &Value) -> Result<(), AppError> {
    if openai_chat_choice_count(request)? == 1 {
        Ok(())
    } else {
        Err(AppError::BadRequest(
            "OpenAI Chat requests must use n = 1".to_owned(),
        ))
    }
}

/// Return the number of Chat completions the upstream is allowed to produce.
///
/// `n` multiplies only completion-side usage. Parsing it before admission
/// avoids creating a reservation that can never cover a valid multi-choice
/// response from a compatible (non-strict) route.
fn openai_chat_choice_count(request: &Value) -> Result<i64, AppError> {
    match request.get("n") {
        None => Ok(1),
        Some(Value::Number(number)) => {
            number.as_i64().filter(|count| *count >= 1).ok_or_else(|| {
                AppError::BadRequest("OpenAI Chat n must be a positive integer".to_owned())
            })
        }
        Some(_) => Err(AppError::BadRequest(
            "OpenAI Chat n must be a positive integer".to_owned(),
        )),
    }
}

fn requires_strict_openai_chat_usage(
    protocol: Protocol,
    route_driver: &str,
    route_config: &Value,
    request: &Value,
) -> bool {
    if route_driver == crate::oauth::managed::kimi::PROVIDER_DRIVER {
        // Native Kimi always requests terminal usage, including when omitted
        // downstream. Do not make billing validation a customer opt-in.
        return matches!(protocol, Protocol::OpenAiChat)
            && request.get("stream").and_then(Value::as_bool) == Some(true);
    }
    matches!(protocol, Protocol::OpenAiChat)
        && crate::provider::is_openai_compatible_http_driver(route_driver)
        && ChatSseUsageContract::from_route_config(route_config).requires_terminal_usage()
        && request.get("stream").and_then(Value::as_bool) == Some(true)
        && request
            .pointer("/stream_options/include_usage")
            .and_then(Value::as_bool)
            == Some(true)
}

struct AuthorizedProxyRoutes {
    primary: PlannedProxyRoute,
    remaining_candidates: std::vec::IntoIter<AuthorizedUpstreamCandidate>,
    input_token_ceiling: i64,
    output_token_ceiling: i64,
    output_choice_count: i64,
}

struct AuthorizedProxyRoutesInput<'a> {
    request: ProxyRequestContext<'a>,
    original_body_length: usize,
    candidates: Vec<AuthorizedUpstreamCandidate>,
}

impl AuthorizedProxyRoutes {
    fn primary_route(&self) -> &ResolvedUpstream {
        &self.primary.route
    }
}

async fn prepare_authorized_proxy_routes(
    input: AuthorizedProxyRoutesInput<'_>,
) -> Result<AuthorizedProxyRoutes, AppError> {
    let AuthorizedProxyRoutesInput {
        request,
        original_body_length,
        candidates,
    } = input;
    let protocol = request.protocol;
    let candidate_preparation = proxy_diagnostics::Phase::new(
        proxy_diagnostics::Context::for_request(request.request_id),
        "authorized_candidate_preparation",
    );
    let request_json = request.request_json;
    let openai_chat_choice_count = matches!(protocol, Protocol::OpenAiChat)
        .then(|| openai_chat_choice_count(request_json))
        .transpose()?;
    let output_choice_count = openai_chat_choice_count.unwrap_or(1);
    let strict_choice_count_is_incompatible =
        openai_chat_choice_count.is_some_and(|count| count != 1);
    let mut remaining_candidates = candidates.into_iter();
    let mut summary = CandidatePreparationSummary::default();
    let Some(primary) = next_planned_proxy_candidate(
        request,
        &mut remaining_candidates,
        strict_choice_count_is_incompatible,
        &mut summary,
    )
    .await?
    else {
        // Normalized grants are the sole downstream authorization source. A
        // missing route must never fall back to unscoped process secrets.
        return Err(exhausted_candidate_error(protocol, request_json, &summary)?);
    };
    candidate_preparation.finish("completed", None, None);
    let (input_token_ceiling, output_token_ceiling) =
        candidate_reservation_bounds(&primary, original_body_length, output_choice_count)?;
    Ok(AuthorizedProxyRoutes {
        primary,
        remaining_candidates,
        input_token_ceiling,
        output_token_ceiling,
        output_choice_count,
    })
}

async fn next_sendable_proxy_route(
    input: NextSendableProxyRouteInput<'_>,
) -> Result<Option<(PreparedProxyRoute, UpstreamAttemptGuard, usize, usize)>, AppError> {
    let NextSendableProxyRouteInput {
        request,
        price,
        reservation,
        input_token_ceiling,
        output_token_ceiling,
        original_body_length,
        output_choice_count,
        assigned_route,
        planned_candidate,
        candidates,
        mut failover_reason,
        candidate_rank,
        outbound_attempts,
        recovery_wait_deadline,
        deferred_shared_probes,
    } = input;
    let state = request.state;
    let request_id = request.request_id;
    let outbound_attempt = outbound_attempts.saturating_add(1);
    let strict_choice_count_is_incompatible = matches!(request.protocol, Protocol::OpenAiChat)
        && openai_chat_choice_count(request.request_json)? != 1;
    let mut summary = CandidatePreparationSummary::default();
    let mut transient_candidate = None;
    while let Some(mut planned) = match planned_candidate.take() {
        Some(planned) => Some(planned),
        None => {
            next_planned_proxy_candidate(
                request,
                candidates,
                strict_choice_count_is_incompatible,
                &mut summary,
            )
            .await?
        }
    } {
        *candidate_rank = (*candidate_rank).saturating_add(1);
        let rank = *candidate_rank;
        if planned.is_component() {
            // Component hooks can have external effects and are only eligible
            // as the initial selected candidate.
            tracing::warn!(
                %request_id,
                upstream_account_id = %planned.route.account_id,
                candidate_rank = rank,
                stage = "component_standby_skip",
                "proxy skipped a component standby after direct routing began"
            );
            continue;
        }
        if refresh_route_snapshot(state, &mut planned.route).await? != PreparedRouteReadiness::Ready
        {
            tracing::warn!(
                %request_id,
                upstream_account_id = %planned.route.account_id,
                candidate_rank = rank,
                outbound_attempt,
                admission_reason = "route_snapshot_unavailable",
                stage = "upstream_admission_skip",
                "proxy skipped an authorized upstream before sending"
            );
            state.metrics.observe_upstream_health(
                UpstreamHealthEvent::Skipped,
                UpstreamHealthReason::Unavailable,
            );
            failover_reason = Some(UpstreamHealthReason::Unavailable);
            continue;
        }
        let preparation_now = unix_millis();
        if planned
            .route
            .credential
            .expires_at()
            .is_some_and(|expires_at| expires_at <= preparation_now)
        {
            tracing::warn!(
                %request_id,
                upstream_account_id = %planned.route.account_id,
                candidate_rank = rank,
                outbound_attempt,
                admission_reason = "credential_expired",
                stage = "upstream_admission_skip",
                "proxy skipped an authorized upstream before sending"
            );
            state.metrics.observe_upstream_health(
                UpstreamHealthEvent::Skipped,
                UpstreamHealthReason::Unavailable,
            );
            failover_reason = Some(UpstreamHealthReason::Unavailable);
            continue;
        }
        let (next_input_token_ceiling, next_output_token_ceiling) =
            candidate_reservation_bounds(&planned, original_body_length, output_choice_count)?;
        let admission = if let Some(snapshot) = state.group_routing.as_ref()
            && let Some(policy) = snapshot.policy(
                planned.route.route_id,
                planned.route.account_id,
                planned.route.credential_generation,
            ) {
            state
                .db
                .claim_upstream_account_attempt_with_strategy(
                    snapshot.tenant_id,
                    planned.route.account_id,
                    planned.route.credential_generation,
                    state.config.upstream_health,
                    policy.allow_probe(),
                    Some(policy.cooldown_ms()),
                    false,
                )
                .await?
        } else {
            state
                .db
                .claim_upstream_account_attempt_with_health_config(
                    planned.route.account_id,
                    planned.route.credential_generation,
                    state.config.upstream_health,
                )
                .await?
        };
        if let UpstreamAttemptAdmission::Unavailable {
            cooldown_until,
            probe_lease_until,
            shared_probe_eligible,
            transient_wait_eligible,
        } = admission
        {
            let now = unix_millis();
            let admission_reason = if cooldown_until > now {
                "cooldown"
            } else if probe_lease_until > now {
                "probe_lease"
            } else {
                "claim_contention"
            };
            tracing::warn!(
                %request_id,
                upstream_account_id = %planned.route.account_id,
                candidate_rank = rank,
                outbound_attempt,
                admission_reason,
                cooldown_until,
                probe_lease_until,
                stage = "upstream_admission_skip",
                "proxy skipped an authorized upstream before sending"
            );
            if shared_probe_eligible {
                deferred_shared_probes.push_back(DeferredSharedProbe {
                    route: planned.route.clone(),
                    candidate_rank: rank,
                    probe_lease_until,
                });
            }
            // Retain at most one small route snapshot, never another request body.
            if outbound_attempts == 0 && transient_wait_eligible && transient_candidate.is_none() {
                transient_candidate = Some((planned.route.clone(), rank));
            }
            state.metrics.observe_upstream_health(
                UpstreamHealthEvent::Skipped,
                UpstreamHealthReason::Cooldown,
            );
            failover_reason = Some(UpstreamHealthReason::Cooldown);
            continue;
        }
        return prepare_admitted_proxy_route(AdmittedProxyRouteInput {
            request,
            price,
            reservation,
            input_token_ceiling,
            output_token_ceiling,
            next_input_token_ceiling,
            next_output_token_ceiling,
            assigned_route,
            failover_reason,
            planned,
            admission,
            existing_guard: None,
            shared_probe_permit: None,
            candidate_rank: rank,
            outbound_attempt,
        })
        .await
        .map(Some);
    }
    while let Some(mut deferred) = deferred_shared_probes.pop_front() {
        if refresh_route_snapshot(state, &mut deferred.route).await?
            != PreparedRouteReadiness::Ready
        {
            tracing::warn!(
                %request_id,
                upstream_account_id = %deferred.route.account_id,
                candidate_rank = deferred.candidate_rank,
                outbound_attempt,
                admission_reason = "deferred_route_snapshot_unavailable",
                stage = "upstream_admission_skip",
                "proxy skipped a stale deferred recovery route before sending"
            );
            continue;
        }
        let preparation_now = unix_millis();
        if deferred
            .route
            .credential
            .expires_at()
            .is_some_and(|expires_at| expires_at <= preparation_now)
        {
            tracing::warn!(
                %request_id,
                upstream_account_id = %deferred.route.account_id,
                candidate_rank = deferred.candidate_rank,
                outbound_attempt,
                admission_reason = "deferred_credential_expired",
                stage = "upstream_admission_skip",
                "proxy skipped an expired deferred recovery route before sending"
            );
            continue;
        }
        let planned = plan_proxy_route(ProxyRoutePlanInput {
            request,
            route: deferred.route,
            preparation_now,
        })?;
        if planned.is_component() {
            return Err(AppError::Internal);
        }
        let (next_input_token_ceiling, next_output_token_ceiling) =
            candidate_reservation_bounds(&planned, original_body_length, output_choice_count)?;
        let transport_policy = routing::runtime_transport_policy(
            &planned.route.config,
            state.config.upstream_health.shared_probe_attempts,
        )?;
        match routing::join_shared_probe(
            state,
            planned.route.account_id,
            planned.route.credential_generation,
            transport_policy.shared_probe_attempts,
        )
        .await?
        {
            Some((admission, permit)) => {
                tracing::warn!(
                    %request_id,
                    upstream_account_id = %planned.route.account_id,
                    candidate_rank = deferred.candidate_rank,
                    outbound_attempt,
                    probe_lease_until = deferred.probe_lease_until,
                    shared_probe_limit = transport_policy.shared_probe_attempts,
                    transport_policy_source = transport_policy.source,
                    stage = "upstream_shared_probe_admission",
                    "proxy admitted bounded recovery traffic after exhausting other candidates"
                );
                return prepare_admitted_proxy_route(AdmittedProxyRouteInput {
                    request,
                    price,
                    reservation,
                    input_token_ceiling,
                    output_token_ceiling,
                    next_input_token_ceiling,
                    next_output_token_ceiling,
                    assigned_route,
                    failover_reason,
                    planned,
                    admission,
                    existing_guard: None,
                    shared_probe_permit: Some(permit),
                    candidate_rank: deferred.candidate_rank,
                    outbound_attempt,
                })
                .await
                .map(Some);
            }
            None => {
                tracing::warn!(
                    %request_id,
                    upstream_account_id = %planned.route.account_id,
                    candidate_rank = deferred.candidate_rank,
                    outbound_attempt,
                    shared_probe_limit = transport_policy.shared_probe_attempts,
                    transport_policy_source = transport_policy.source,
                    stage = "upstream_shared_probe_rejected",
                    "bounded shared probe capacity is unavailable"
                );
            }
        }
    }
    if let Some((route, rank)) = transient_candidate
        && let Some((route, admission, guard)) =
            routing::recovery_wait::wait(state, request_id, route, recovery_wait_deadline).await?
    {
        let prepared = async {
            let planned = plan_proxy_route(ProxyRoutePlanInput {
                request,
                route,
                preparation_now: unix_millis(),
            })?;
            let (next_input, next_output) =
                candidate_reservation_bounds(&planned, original_body_length, output_choice_count)?;
            prepare_admitted_proxy_route(AdmittedProxyRouteInput {
                request,
                price,
                reservation,
                input_token_ceiling,
                output_token_ceiling,
                next_input_token_ceiling: next_input,
                next_output_token_ceiling: next_output,
                assigned_route,
                failover_reason,
                planned,
                admission,
                existing_guard: Some(guard),
                shared_probe_permit: None,
                candidate_rank: rank,
                outbound_attempt,
            })
            .await
        }
        .await;
        return prepared.map(Some);
    }
    Ok(None)
}

struct NonSseProxyResponseInput<'buffered, 'state, 'attempt> {
    buffered_request: &'buffered BufferedRequest<'state>,
    upstream: UpstreamResponse,
    status: StatusCode,
    content_type: Option<HeaderValue>,
    protocol: Protocol,
    capture_json_usage: bool,
    input_token_ceiling: i64,
    output_token_ceiling: i64,
    upstream_attempt: &'attempt mut UpstreamAttemptGuard,
}

async fn finish_non_sse_proxy_response(
    input: NonSseProxyResponseInput<'_, '_, '_>,
) -> Result<Response, AppError> {
    let NonSseProxyResponseInput {
        buffered_request,
        upstream,
        status,
        content_type,
        protocol,
        capture_json_usage,
        input_token_ceiling,
        output_token_ceiling,
        upstream_attempt,
    } = input;
    let response_content_type = content_type
        .as_ref()
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json")
        .to_owned();
    let buffer_phase = proxy_diagnostics::Phase::new(
        proxy_diagnostics::Context::for_request(buffered_request.request_id),
        "buffered_response",
    );
    let response_body = match read_bounded_upstream(
        upstream,
        MAX_PROXY_RESPONSE_BODY,
        &buffered_request.memory,
        buffered_request.started,
        false,
    )
    .await
    {
        Ok(body) => Bytes::from(body),
        Err(error) => {
            buffer_phase.finish(error.code(), Some(status.as_u16()), None);
            let result = finish_proxy_failure(buffered_request, error.code()).await;
            upstream_attempt
                .complete(UpstreamAttemptTerminal::invalid_response())
                .await;
            return result;
        }
    };
    buffer_phase.finish(
        "completed",
        Some(status.as_u16()),
        Some(response_body.len()),
    );
    let validation = match protocol {
        Protocol::OpenAiChat => validate_buffered_chat_success(&response_body),
        Protocol::OpenAiResponses => validate_buffered_responses_success(&response_body),
        _ => Ok(()),
    };
    if let Err(error_code) = validation {
        let result = finish_proxy_failure(buffered_request, error_code).await;
        upstream_attempt
            .complete(UpstreamAttemptTerminal::invalid_response())
            .await;
        return result;
    }
    let usage = if capture_json_usage {
        match extract_usage_checked(&response_body) {
            ExtractedUsage::Valid(usage) => {
                (usage, crate::model::RequestUsageBasis::ProviderReported)
            }
            ExtractedUsage::Missing => (
                TokenUsage {
                    input_tokens: input_token_ceiling,
                    output_tokens: output_token_ceiling,
                    ..TokenUsage::default()
                },
                crate::model::RequestUsageBasis::ContractCeiling,
            ),
            ExtractedUsage::Invalid => {
                let result = finish_proxy_failure(buffered_request, "upstream_invalid_usage").await;
                upstream_attempt
                    .complete(UpstreamAttemptTerminal::invalid_response())
                    .await;
                return result;
            }
        }
    } else {
        (
            TokenUsage {
                input_tokens: input_token_ceiling,
                output_tokens: output_token_ceiling,
                ..TokenUsage::default()
            },
            crate::model::RequestUsageBasis::ContractCeiling,
        )
    };
    let result = finish_buffered_request(
        buffered_request,
        status,
        response_body,
        &response_content_type,
        usage,
        None,
    )
    .await;
    let attempt_terminal = match result.as_ref() {
        Ok(response) if response.status().is_success() => UpstreamAttemptTerminal::Succeeded,
        Ok(_) => UpstreamAttemptTerminal::invalid_response(),
        Err(_) => UpstreamAttemptTerminal::Inconclusive,
    };
    upstream_attempt.complete(attempt_terminal).await;
    result
}

fn requested_service_tier(
    request_json: &Value,
    price: &crate::model::ModelPrice,
) -> Result<Option<String>, AppError> {
    let requested = match request_json.get("service_tier") {
        None => None,
        Some(Value::String(tier)) if is_supported_service_tier(tier) => Some(tier.clone()),
        Some(_) => {
            return Err(AppError::BadRequest(
                "service_tier must be default, auto, priority, flex, scale, batch, or standard_only"
                    .into(),
            ));
        }
    };
    if let Some(tier) = requested.as_deref()
        && !matches!(tier, "auto" | "standard_only")
        && !(tier == "default" && price.tiers.is_empty())
        && !price
            .tiers
            .iter()
            .any(|price_tier| price_tier.service_tier == tier)
    {
        return Err(AppError::BadRequest(
            "the requested service_tier has no configured price".into(),
        ));
    }
    Ok(requested)
}

async fn execute_component_primary(
    mut request: BufferedRequest<'_>,
    key: &AuthenticatedKey,
    price: &crate::model::ModelPrice,
    mut primary: PlannedProxyRoute,
    original_body_length: usize,
    recovery_wait_deadline: tokio::time::Instant,
) -> Result<Response, AppError> {
    let readiness = match refresh_route_snapshot(request.state, &mut primary.route).await {
        Ok(readiness) => readiness,
        Err(error) => {
            tracing::warn!(
                request_id = %request.request_id,
                upstream_account_id = %primary.route.account_id,
                error_category = error.diagnostic_category(),
                "current upstream credential is invalid"
            );
            return finish_proxy_failure(&request, "upstream_credential_invalid").await;
        }
    };
    if readiness != PreparedRouteReadiness::Ready {
        return finish_unavailable(&request, readiness.error_code(), None).await;
    }
    // Every component request passes core health admission, including failed
    // strategy execution/native fallback. A missing policy never disables the
    // generation fence or revives hard quota. This path never replays a send.
    let admission = if let Some(snapshot) = request.state.group_routing.as_ref()
        && let Some(policy) = snapshot.policy(
            primary.route.route_id,
            primary.route.account_id,
            primary.route.credential_generation,
        ) {
        request
            .state
            .db
            .claim_upstream_account_attempt_with_strategy(
                snapshot.tenant_id,
                primary.route.account_id,
                primary.route.credential_generation,
                request.state.config.upstream_health,
                policy.allow_probe(),
                Some(policy.cooldown_ms()),
                false,
            )
            .await?
    } else {
        request
            .state
            .db
            .claim_upstream_account_attempt_with_health_config(
                primary.route.account_id,
                primary.route.credential_generation,
                request.state.config.upstream_health,
            )
            .await?
    };
    let upstream_attempt = match admission {
        UpstreamAttemptAdmission::Unavailable {
            transient_wait_eligible: true,
            ..
        } => {
            let Some((route, _, guard)) = routing::recovery_wait::wait(
                request.state,
                request.request_id,
                primary.route.clone(),
                recovery_wait_deadline,
            )
            .await?
            else {
                return finish_unavailable(&request, "upstream_unavailable", None).await;
            };
            primary.route = route;
            Some(guard)
        }
        UpstreamAttemptAdmission::Unavailable { .. } => {
            return finish_unavailable(&request, "upstream_unavailable", None).await;
        }
        admission => Some(UpstreamAttemptGuard::new(
            request.state,
            request.request_id,
            primary.route.route_id,
            primary.route.account_id,
            primary.route.credential_generation,
            admission,
            None,
        )),
    };
    let mut active_route = match materialize_proxy_route(request.state, primary).await {
        Ok(prepared) => prepared,
        Err(_) => return finish_proxy_failure(&request, "provider_candidate_invalid").await,
    };
    let next_input_token_ceiling =
        match prepared_input_reservation_bound(&active_route, original_body_length) {
            Ok(ceiling) => ceiling,
            Err(_) => return finish_proxy_failure(&request, "provider_candidate_invalid").await,
        };
    if next_input_token_ceiling != request.input_token_ceiling {
        let assignment = (active_route.route.account_id, active_route.route.route_id);
        let resized = match request
            .state
            .db
            .switch_pending_proxy_candidate(SwitchProxyCandidateInput {
                request_id: request.request_id,
                tenant_id: request.tenant_id,
                key,
                price,
                reservation: &request.reservation,
                input_token_ceiling: next_input_token_ceiling,
                output_token_ceiling: request.output_token_ceiling,
                expected_assignment: assignment,
                next_assignment: assignment,
            })
            .await
        {
            Ok(resized) => resized,
            Err(_) => return finish_proxy_failure(&request, "provider_candidate_invalid").await,
        };
        request.reservation = resized;
        request.input_token_ceiling = next_input_token_ceiling;
    }
    let Some((prepared, component_context)) = active_route.component_request.take() else {
        return finish_proxy_failure(&request, "provider_candidate_invalid").await;
    };
    active_route.release_request_buffers();
    execute_component_provider(
        request,
        &active_route.route.driver,
        &active_route.route.base_url,
        &active_route.route.config,
        &active_route.route.credential,
        prepared,
        component_context,
        upstream_attempt,
    )
    .await
}

pub(super) async fn proxy(
    state: AppState,
    headers: HeaderMap,
    body: Bytes,
    protocol: Protocol,
    memory: std::sync::Arc<crate::gateway_body::memory::ProxyMemoryReservation>,
) -> Result<Response, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    proxy_with_identity(state, headers, body, protocol, key, None, memory).await
}

/// Internal callers must establish an explicit billing identity. A pinned route
/// narrows normal grants; it never grants access or falls back to another route.
pub(in crate::api) async fn proxy_with_identity(
    state: AppState,
    headers: HeaderMap,
    body: Bytes,
    protocol: Protocol,
    key: AuthenticatedKey,
    pinned_route: Option<Uuid>,
    memory: std::sync::Arc<crate::gateway_body::memory::ProxyMemoryReservation>,
) -> Result<Response, AppError> {
    let diagnostic_context = proxy_diagnostics::Context::current();
    let request_id = diagnostic_context.request_id;
    let preparation = proxy_diagnostics::Phase::new(diagnostic_context, "request_preparation");
    let _request_buffer = state
        .metrics
        .memory_usage(crate::metrics::MemoryComponent::RequestBuffer, body.len());
    let plugin_snapshot =
        proxy_diagnostics::Phase::new(diagnostic_context, "application_plugin_snapshot");
    let mut state = state.pin_application_plugins().await?;
    plugin_snapshot.finish("completed", None, None);
    let proxy_lifecycle_permit = state
        .proxy_lifecycle_permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| AppError::Overloaded)?;
    if !memory.try_reserve_json(&body) {
        state
            .metrics
            .record_proxy_memory_rejection(crate::metrics::ProxyMemoryRejectionStage::Json);
        return Err(AppError::Overloaded);
    }
    let json_parse = proxy_diagnostics::Phase::new(diagnostic_context, "request_json_parse");
    let original_request_json: Value = serde_json::from_slice(&body)
        .map_err(|_| AppError::BadRequest("request body must be valid JSON".into()))?;
    json_parse.finish("completed", None, Some(body.len()));
    let conversation_hints = conversation_hints(&headers, &original_request_json);
    tracing::info!(%request_id, phase = "request_shape", bytes = body.len(), compaction_hint = conversation_hints.compaction, "proxy request metadata");
    let traffic_policy =
        proxy_diagnostics::Phase::new(diagnostic_context, "request_traffic_policy");
    let applied = super::traffic::apply_traffic_policy_with_memory(
        &state,
        &key,
        TrafficPolicyProtocols::same(protocol.name()),
        original_request_json.clone(),
        memory.clone(),
    )
    .await?;
    traffic_policy.finish("completed", None, None);
    if pinned_route.is_some() && applied.changes_pinned_envelope(&original_request_json) {
        // Pinned internal callers establish their own reviewed request
        // envelope. Traffic policy may still deny it or rank an already
        // authorized account, but must never add tools, replace instructions,
        // redirect model input, enable streaming, or alter token ceilings.
        // Keep the diagnostic fixed and exclude both request bodies.
        tracing::warn!(
            %request_id,
            stage = "pinned_request_rewrite_rejected",
            "traffic policy attempted to rewrite a pinned internal request"
        );
        return Err(AppError::Forbidden);
    }
    let request_json = applied.request_json;
    let model = applied.model;
    preparation.finish("completed", None, Some(body.len()));
    let route_preparation = proxy_diagnostics::Phase::new(diagnostic_context, "route_preparation");
    let selection_seed = routing_selection_seed(&key, request_id, &conversation_hints);
    let candidate_query =
        proxy_diagnostics::Phase::new(diagnostic_context, "authorized_candidate_query");
    let mut candidates = state
        .db
        .list_authorized_upstream_candidates_with_hint(
            key.key_id,
            key.tenant_id,
            &model,
            protocol.name(),
            RouteSelectionOptions {
                upstream_account_hint: applied.upstream_account_hint,
                selection_seed,
            },
        )
        .await?;
    candidate_query.finish("completed", None, None);
    retain_pinned_text_candidates(&state, pinned_route, &mut candidates)?;
    let strategy_candidates =
        crate::group_routing::candidate_snapshot_if_enabled(&state, &candidates);
    let request_context = ProxyRequestContext {
        state: &state,
        key: &key,
        model: &model,
        protocol,
        request_id,
        request_json: &request_json,
    };
    let mut route_plan = prepare_authorized_proxy_routes(AuthorizedProxyRoutesInput {
        request: request_context,
        original_body_length: body.len(),
        candidates,
    })
    .await?;
    let primary = route_plan.primary_route();
    // Freeze before reservation and archive work: later candidates/reloads may
    // change account transport settings, never replenish the request budget.
    let attempt_budget = routing::RequestAttemptBudget::from_primary(primary, request_id)?;
    let recovery_wait_deadline =
        attempt_budget.recovery_wait_deadline(state.config.upstream_health);
    if let Some(mut candidates) = strategy_candidates {
        let strategy =
            proxy_diagnostics::Phase::new(diagnostic_context, "group_routing_preparation");
        crate::group_routing::prepare(
            &mut state,
            key.tenant_id,
            selection_seed,
            request_id,
            recovery_wait_deadline,
            &mut candidates,
        )
        .await?;
        strategy.finish("completed", None, None);
        if state.group_routing.is_some() {
            route_plan = prepare_authorized_proxy_routes(AuthorizedProxyRoutesInput {
                request: ProxyRequestContext {
                    state: &state,
                    key: &key,
                    model: &model,
                    protocol,
                    request_id,
                    request_json: &request_json,
                },
                original_body_length: body.len(),
                candidates,
            })
            .await?;
        }
    }
    let request_context = ProxyRequestContext {
        state: &state,
        key: &key,
        model: &model,
        protocol,
        request_id,
        request_json: &request_json,
    };
    let primary = route_plan.primary_route();
    let upstream_account_id = Some(primary.account_id);
    let model_route_id = Some(primary.route_id);
    let price_lookup = proxy_diagnostics::Phase::new(diagnostic_context, "model_price_lookup");
    let price = state.db.model_price(&model, &key.currency).await?;
    price_lookup.finish("completed", None, None);
    let input_token_ceiling = route_plan.input_token_ceiling;
    let output_token_ceiling = route_plan.output_token_ceiling;
    let requested_service_tier = requested_service_tier(&request_json, &price)?;
    route_preparation.finish("completed", None, None);
    let admission = proxy_diagnostics::Phase::account(
        diagnostic_context,
        "request_archive_admission",
        upstream_account_id,
        Some(primary.credential_generation),
    );
    let admitted_request_object = format!("gap://{request_id}/request");
    let request_capture_memory = state.metrics.memory_usage(
        crate::metrics::MemoryComponent::StreamCapture,
        body.len().saturating_mul(3),
    );
    let reservation = match state
        .db
        .start_proxy_request_with_archive_compression(
            StartProxyRequest {
                request_id,
                key: &key,
                price: &price,
                input_token_ceiling,
                output_token_ceiling,
                protocol: protocol.name(),
                model: &model,
                request_object: &admitted_request_object,
                upstream_account_id,
                model_route_id,
            },
            &body,
            state.config.key_pepper.as_bytes(),
            state.config.archive_spool_compression_enabled,
        )
        .await
    {
        Ok(reservation) => reservation,
        Err(error) => {
            admission.finish(error.diagnostic_category(), None, Some(body.len()));
            tracing::error!(%request_id, stage = "request_transaction_admission", failure_domain = "local_admission", error_category = error.diagnostic_category(), "proxy request admission failed");
            return Err(error);
        }
    };
    admission.finish("completed", None, Some(body.len()));
    drop(request_capture_memory);
    memory.release(
        body.len(),
        crate::gateway_body::memory::CAPTURE_MEMORY_WEIGHT,
    );
    let request_body_length = body.len();
    drop(body);
    let client_name = client_name(&headers);
    let conversation = matches!(
        protocol,
        Protocol::OpenAiChat | Protocol::OpenAiResponses | Protocol::AnthropicMessages
    )
    .then(|| ProxyConversation {
        key: key.clone(),
        request_json: original_request_json,
        hints: conversation_hints,
        client_name,
    });
    let mut buffered_request = BufferedRequest {
        state: &state,
        reservation,
        request_id,
        started: Instant::now(),
        input_token_ceiling,
        output_token_ceiling,
        requested_service_tier,
        conversation,
        protocol,
        tenant_id: key.tenant_id,
        memory,
    };
    // Admission ACK includes reservation, request record, and encrypted sealed
    // request spool in one transaction. No upstream work starts before it.
    // A requested stream can still return a successful JSON envelope, so it
    // needs the buffered-response safety partition until the response headers
    // prove that the actual downstream path is SSE. Waiting here is bounded,
    // FIFO, and occurs after durable admission but before any upstream send.
    let retained_admission =
        proxy_diagnostics::Phase::new(diagnostic_context, "retained_memory_admission");
    if !buffered_request
        .memory
        .finalize_request(tokio::time::Instant::now() + RETAINED_REQUEST_ADMISSION_WAIT)
        .await
    {
        retained_admission.finish("rejected", Some(503), None);
        state
            .metrics
            .record_proxy_memory_rejection(crate::metrics::ProxyMemoryRejectionStage::Retained);
        let mut response = finish_buffered_request(
            &buffered_request,
            StatusCode::SERVICE_UNAVAILABLE,
            Bytes::from_static(
                b"{\"error\":{\"message\":\"gateway memory capacity unavailable\"}}",
            ),
            "application/json",
            (
                TokenUsage::default(),
                crate::model::RequestUsageBasis::NotObserved,
            ),
            Some("proxy_memory_capacity".to_owned()),
        )
        .await?;
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        return Ok(response);
    }
    retained_admission.finish("completed", None, None);
    let AuthorizedProxyRoutes {
        primary,
        remaining_candidates,
        input_token_ceiling: _,
        output_token_ceiling: _,
        output_choice_count,
    } = route_plan;
    if primary.is_component() {
        return execute_component_primary(
            buffered_request,
            &key,
            &price,
            primary,
            request_body_length,
            recovery_wait_deadline,
        )
        .await;
    }
    let mut planned_candidate = Some(primary);
    let mut route_candidates = remaining_candidates;
    let mut assigned_route = (
        upstream_account_id.ok_or(AppError::Internal)?,
        model_route_id.ok_or(AppError::Internal)?,
    );
    let mut outbound_attempts = 0_usize;
    let mut last_dispatch = None;
    let mut candidate_rank = 0_usize;
    let mut deferred_shared_probes = std::collections::VecDeque::new();
    let mut next_failover_reason = None;
    let (mut active_route, upstream, upstream_activity, mut codex_retry, mut upstream_attempt) = loop {
        if let Some(reason) = attempt_budget.terminal_reason(outbound_attempts) {
            tracing::warn!(%request_id, outbound_attempts, stage = reason,
                policy_version = attempt_budget.version, "proxy request budget exhausted");
            return finish_unavailable(&buffered_request, reason, last_dispatch).await;
        }
        let selection = proxy_diagnostics::Phase::new(diagnostic_context, "candidate_selection");
        let selected = match next_sendable_proxy_route(NextSendableProxyRouteInput {
            request: request_context,
            price: &price,
            reservation: &mut buffered_request.reservation,
            input_token_ceiling: &mut buffered_request.input_token_ceiling,
            output_token_ceiling: &mut buffered_request.output_token_ceiling,
            original_body_length: request_body_length,
            output_choice_count,
            assigned_route: &mut assigned_route,
            planned_candidate: &mut planned_candidate,
            candidates: &mut route_candidates,
            failover_reason: next_failover_reason.take(),
            candidate_rank: &mut candidate_rank,
            outbound_attempts,
            recovery_wait_deadline,
            deferred_shared_probes: &mut deferred_shared_probes,
        })
        .await
        {
            Ok(selected) => selected,
            Err(error) => {
                selection.finish(error.diagnostic_category(), None, None);
                tracing::warn!(
                    %request_id,
                    error_category = error.diagnostic_category(),
                    failure_domain = "local_admission",
                    stage = "upstream_candidate_selection",
                    "proxy candidate selection failed"
                );
                return finish_proxy_failure(&buffered_request, "upstream_candidate_invalid").await;
            }
        };
        selection.finish(
            if selected.is_some() {
                "completed"
            } else {
                "unavailable"
            },
            None,
            None,
        );
        let Some((active_route, mut upstream_attempt, selected_candidate_rank, outbound_attempt)) =
            selected
        else {
            return finish_unavailable(&buffered_request, "upstream_unavailable", last_dispatch)
                .await;
        };
        // Selection may have waited for database admission; do not dispatch
        // when the original deadline expired during that wait.
        if let Some(reason) = attempt_budget.terminal_reason(outbound_attempts) {
            tracing::warn!(%request_id, outbound_attempts, stage = reason,
                failure_domain = "local_admission", delivery_evidence = "candidate_not_dispatched",
                "proxy request budget expired during candidate selection");
            upstream_attempt
                .complete(UpstreamAttemptTerminal::Inconclusive)
                .await;
            return finish_unavailable(&buffered_request, reason, last_dispatch).await;
        }
        let attempt = proxy_diagnostics::Phase::account(
            diagnostic_context,
            "upstream_response_admission",
            Some(active_route.route.account_id),
            Some(active_route.route.credential_generation),
        );
        let (result, rate_limit) = match attempt_budget
            .send(send_proxy_route(
                &state,
                &headers,
                protocol,
                request_id,
                &active_route,
                selected_candidate_rank,
                outbound_attempt,
            ))
            .await
        {
            Ok(mut result)
                if active_route.is_codex()
                    && result.response.status() == StatusCode::TOO_MANY_REQUESTS =>
            {
                let (response, kind) = routing::classify_rate_limit(result.response).await;
                result.response = response;
                (Ok(result), Some(kind))
            }
            result => (result, None),
        };
        attempt.finish(
            if result.is_ok() {
                "completed"
            } else {
                "failed"
            },
            result
                .as_ref()
                .ok()
                .map(|response| response.response.status().as_u16()),
            None,
        );
        let consumed_outbound_attempt = !matches!(
            &result,
            Err(ProxySendError::CandidateUnavailable | ProxySendError::CredentialUnavailable)
        );
        if consumed_outbound_attempt {
            outbound_attempts += 1;
            last_dispatch = Some((active_route.route.account_id, active_route.route.route_id));
        }
        let failure = routing::classify_attempt_failure(&result, rate_limit);
        routing::diagnostics::observe_send(
            request_id,
            selected_candidate_rank,
            outbound_attempt,
            &result,
        );
        let candidate_unavailable = matches!(
            &result,
            Err(ProxySendError::CandidateUnavailable | ProxySendError::CredentialUnavailable)
        );
        if candidate_unavailable {
            tracing::warn!(
                %request_id,
                upstream_account_id = %active_route.route.account_id,
                candidate_rank = selected_candidate_rank,
                outbound_attempt,
                send_error = ?result.as_ref().err(),
                stage = "upstream_send_preparation_skip",
                "proxy candidate became unusable before an outbound attempt"
            );
            upstream_attempt
                .complete(UpstreamAttemptTerminal::Inconclusive)
                .await;
            state.metrics.observe_upstream_health(
                UpstreamHealthEvent::Skipped,
                UpstreamHealthReason::Unavailable,
            );
        }
        if let Some((kind, reason)) = failure {
            let status = result
                .as_ref()
                .ok()
                .map(|result| result.response.status().as_u16());
            tracing::warn!(
                %request_id,
                upstream_account_id = %active_route.route.account_id,
                candidate_rank = selected_candidate_rank,
                outbound_attempt,
                failure_kind = kind.as_str(),
                ?status,
                send_error = ?result.as_ref().err(),
                stage = "upstream_send_failure",
                "proxy upstream attempt failed"
            );
            upstream_attempt
                .complete(UpstreamAttemptTerminal::Failed { kind, reason })
                .await;
        }
        let disposition = routing::failover_disposition(
            result.as_ref().ok().map(|result| result.response.status()),
            result.as_ref().err(),
        );
        let has_standby =
            !route_candidates.as_slice().is_empty() || !deferred_shared_probes.is_empty();
        let failover_reason = disposition.reason().filter(|_| has_standby);
        if !result
            .as_ref()
            .is_ok_and(|result| result.response.status().is_success())
        {
            tracing::warn!(%request_id, route_id = %active_route.route.route_id,
                upstream_account_id = %active_route.route.account_id,
                candidate_rank = selected_candidate_rank, outbound_attempt,
                policy_version = attempt_budget.version,
                disposition = disposition.as_str(), has_standby,
                budget_terminal = attempt_budget.terminal_reason(outbound_attempts),
                stage = "upstream_failover_decision", "proxy evaluated delivery evidence");
        }
        if let Some(reason) = failover_reason
            && (!consumed_outbound_attempt
                || attempt_budget.terminal_reason(outbound_attempts).is_none())
        {
            next_failover_reason = Some(reason);
            continue;
        }
        match result {
            Ok(result) => {
                break (
                    active_route,
                    result.response,
                    result.upstream_activity,
                    result.codex_retry,
                    upstream_attempt,
                );
            }
            Err(ProxySendError::Credential) => {
                upstream_attempt
                    .complete(UpstreamAttemptTerminal::invalid_response())
                    .await;
                return finish_proxy_failure(&buffered_request, "provider_credential").await;
            }
            Err(ProxySendError::CredentialUnavailable) => {
                return finish_unavailable(
                    &buffered_request,
                    "upstream_credential_unavailable",
                    last_dispatch,
                )
                .await;
            }
            Err(ProxySendError::RetryableConnection(_) | ProxySendError::CandidateUnavailable) => {
                return finish_unavailable(&buffered_request, "upstream_connection", last_dispatch)
                    .await;
            }
            Err(ProxySendError::RetryableCodexBadRequest) => {
                return finish_unavailable(&buffered_request, "upstream_rejected", last_dispatch)
                    .await;
            }
            Err(ProxySendError::CodexBadRequest) => {
                upstream_attempt
                    .complete(UpstreamAttemptTerminal::Inconclusive)
                    .await;
                return finish_buffered_request(
                    &buffered_request,
                    StatusCode::BAD_REQUEST,
                    Bytes::from_static(
                        b"{\"error\":{\"message\":\"upstream rejected the request\",\"type\":\"upstream_error\"}}",
                    ),
                    "application/json",
                    (
                        TokenUsage::default(),
                        crate::model::RequestUsageBasis::NotObserved,
                    ),
                    Some("http_400".to_owned()),
                )
                .await;
            }
            Err(ProxySendError::AmbiguousResponse(error_code)) => {
                upstream_attempt
                    .complete(UpstreamAttemptTerminal::Inconclusive)
                    .await;
                return finish_proxy_failure(&buffered_request, error_code).await;
            }
            Err(ProxySendError::NonRetryableTransport | ProxySendError::OuterDeadline) => {
                upstream_attempt
                    .complete(UpstreamAttemptTerminal::Inconclusive)
                    .await;
                return finish_proxy_failure(&buffered_request, "upstream_transport").await;
            }
        }
    };
    let strict_openai_chat_usage = requires_strict_openai_chat_usage(
        protocol,
        &active_route.route.driver,
        &active_route.route.config,
        &request_json,
    );
    let codex_chat_include_usage = active_route.is_codex()
        && matches!(protocol, Protocol::OpenAiChat)
        && request_json
            .pointer("/stream_options/include_usage")
            .and_then(Value::as_bool)
            == Some(true);
    drop(request_json);
    active_route.release_request_buffers();
    let is_codex_route = active_route.is_codex();
    let codex_downstream_stream = active_route.codex_downstream_stream;
    let upstream_account_id = Some(active_route.route.account_id);
    let route_driver = Some(active_route.route.driver.as_str());
    let status = upstream.status();
    if !status.is_success() {
        crate::api::trigger_copilot_remint_on_auth_failure(
            &state,
            route_driver,
            upstream_account_id,
            request_id,
            status,
        );
        drop(upstream);
        let result = finish_buffered_request(
            &buffered_request,
            status,
            Bytes::from_static(
                b"{\"error\":{\"message\":\"upstream rejected the request\",\"type\":\"upstream_error\"}}",
            ),
            "application/json",
            (
                TokenUsage::default(),
                crate::model::RequestUsageBasis::NotObserved,
            ),
            Some(format!("http_{}", status.as_u16())),
        )
        .await;
        upstream_attempt
            .complete(if status.is_client_error() {
                // Ordinary caller-dependent 4xx responses are not evidence
                // that a shared upstream account is unhealthy. Typed auth,
                // rate-limit, and transient statuses were handled above.
                UpstreamAttemptTerminal::Inconclusive
            } else {
                UpstreamAttemptTerminal::invalid_response()
            })
            .await;
        codex_retry.complete(CodexRetryTerminal::Failed);
        return result;
    }
    let content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();
    if is_codex_route && !codex_downstream_stream {
        let buffer_phase = proxy_diagnostics::Phase::account(
            diagnostic_context,
            "codex_buffered_response",
            Some(active_route.route.account_id),
            Some(active_route.route.credential_generation),
        );
        let buffered = match codex_transport::buffer_response(
            upstream,
            &buffered_request.memory,
            buffered_request.started,
        )
        .await
        {
            Ok(buffered) => buffered,
            Err(error_code) => {
                buffer_phase.finish(error_code, None, None);
                tracing::warn!(%request_id, stage = error_code, "Codex upstream response failed");
                let result = finish_proxy_failure(&buffered_request, error_code).await;
                upstream_attempt
                    .complete(
                        if matches!(
                            error_code,
                            "upstream_read_timeout" | "upstream_request_timeout"
                        ) {
                            UpstreamAttemptTerminal::Inconclusive
                        } else {
                            UpstreamAttemptTerminal::invalid_response()
                        },
                    )
                    .await;
                codex_retry.complete(CodexRetryTerminal::Failed);
                return result;
            }
        };
        if matches!(protocol, Protocol::OpenAiResponses) && buffered.terminal.is_incomplete() {
            buffer_phase.finish(
                "upstream_incomplete_response",
                Some(200),
                Some(buffered.body.len()),
            );
            let result = finish_buffered_request(
                &buffered_request,
                StatusCode::BAD_GATEWAY,
                Bytes::from_static(
                    b"{\"error\":{\"message\":\"upstream response was incomplete\",\"type\":\"upstream_error\"}}",
                ),
                "application/json",
                (
                    buffered.usage,
                    crate::model::RequestUsageBasis::ProviderReported,
                ),
                Some("upstream_incomplete_response".to_owned()),
            )
            .await;
            upstream_attempt
                .complete(UpstreamAttemptTerminal::Inconclusive)
                .await;
            codex_retry.complete(CodexRetryTerminal::Failed);
            return result;
        }
        let buffered = if matches!(protocol, Protocol::OpenAiChat) {
            match codex_transport::translate_buffered_chat_response(buffered, request_id, &model) {
                Ok(buffered) => buffered,
                Err(error_code) => {
                    buffer_phase.finish(error_code, None, None);
                    tracing::warn!(%request_id, stage = error_code, "Codex Chat response translation failed");
                    let result = finish_proxy_failure(&buffered_request, error_code).await;
                    upstream_attempt
                        .complete(UpstreamAttemptTerminal::invalid_response())
                        .await;
                    codex_retry.complete(CodexRetryTerminal::Failed);
                    return result;
                }
            }
        } else {
            buffered
        };
        buffer_phase.finish("completed", Some(200), Some(buffered.body.len()));
        let result = finish_buffered_request(
            &buffered_request,
            StatusCode::OK,
            buffered.body,
            "application/json",
            (
                buffered.usage,
                crate::model::RequestUsageBasis::ProviderReported,
            ),
            None,
        )
        .await;
        let succeeded = result
            .as_ref()
            .is_ok_and(|response| response.status().is_success());
        let attempt_terminal = match result.as_ref() {
            Ok(response) if response.status().is_success() => UpstreamAttemptTerminal::Succeeded,
            Ok(_) => UpstreamAttemptTerminal::invalid_response(),
            Err(_) => UpstreamAttemptTerminal::Inconclusive,
        };
        upstream_attempt.complete(attempt_terminal).await;
        codex_retry.complete(if succeeded {
            CodexRetryTerminal::Succeeded
        } else {
            CodexRetryTerminal::Failed
        });
        return result;
    }
    let is_sse = content_type
        .as_ref()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"));
    // An opted-in Chat usage stream has a terminal SSE usage contract. A
    // successful JSON envelope cannot prove that contract and must never be
    // forwarded or settled as a compatible buffered response.
    if strict_openai_chat_usage && !is_sse {
        drop(upstream);
        let result = finish_proxy_failure(&buffered_request, "upstream_invalid_response").await;
        upstream_attempt
            .complete(UpstreamAttemptTerminal::invalid_response())
            .await;
        return result;
    }
    let capture_json_usage = should_capture_buffered_usage(is_sse, content_type.as_ref());
    if !is_sse {
        let selected_input_token_ceiling = buffered_request.input_token_ceiling;
        let selected_output_token_ceiling = buffered_request.output_token_ceiling;
        return finish_non_sse_proxy_response(NonSseProxyResponseInput {
            buffered_request: &buffered_request,
            upstream,
            status,
            content_type,
            protocol,
            capture_json_usage,
            input_token_ceiling: selected_input_token_ceiling,
            output_token_ceiling: selected_output_token_ceiling,
            upstream_attempt: &mut upstream_attempt,
        })
        .await;
    }
    buffered_request
        .memory
        .release_retained_request_for_stream();
    streaming::stream_response(streaming::StreamingResponse {
        state: &state,
        upstream,
        status,
        content_type,
        is_sse,
        capture_json_usage,
        protocol,
        is_codex_route,
        codex_retry,
        strict_openai_chat_usage,
        codex_chat_model: (is_codex_route && matches!(protocol, Protocol::OpenAiChat))
            .then(|| model.clone()),
        codex_chat_include_usage,
        upstream_attempt,
        upstream_activity,
        request_id,
        upstream_account_id: active_route.route.account_id,
        credential_generation: active_route.route.credential_generation,
        buffered_request,
        proxy_lifecycle_permit,
    })
    .await
}

fn trusted_input_token_overhead_ceiling(
    route_driver: Option<&str>,
    route_config: Option<&Value>,
) -> Result<i64, AppError> {
    if !route_driver.is_some_and(crate::provider::is_openai_compatible_http_driver) {
        return Ok(0);
    }
    let Some(value) = route_config.and_then(|config| config.get("input_token_overhead_ceiling"))
    else {
        return Ok(0);
    };
    value
        .as_i64()
        .filter(|ceiling| (0..=MAX_INPUT_TOKEN_OVERHEAD_CEILING).contains(ceiling))
        .ok_or_else(|| {
            AppError::Upstream(
                "OpenAI-compatible upstream input token overhead ceiling is invalid".into(),
            )
        })
}

fn validate_buffered_responses_success(body: &[u8]) -> Result<(), &'static str> {
    let value: Value = serde_json::from_slice(body).map_err(|_| "upstream_invalid_response")?;
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return Err("upstream_failed_response");
    }
    match value.get("status") {
        None => Ok(()),
        Some(Value::String(status)) if status == "completed" => Ok(()),
        Some(Value::String(status))
            if matches!(status.as_str(), "failed" | "incomplete" | "cancelled") =>
        {
            Err("upstream_failed_response")
        }
        Some(Value::String(status)) if matches!(status.as_str(), "queued" | "in_progress") => {
            Err("upstream_incomplete_response")
        }
        Some(_) => Err("upstream_invalid_response"),
    }
}

fn validate_buffered_chat_success(body: &[u8]) -> Result<(), &'static str> {
    let value: Value = serde_json::from_slice(body).map_err(|_| "upstream_invalid_response")?;
    if !value.is_object() {
        return Err("upstream_invalid_response");
    }
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return Err("upstream_failed_response");
    }
    Ok(())
}

#[derive(Clone)]
struct ProxyConversation {
    key: AuthenticatedKey,
    request_json: Value,
    hints: crate::conversation::ConversationHints,
    client_name: Option<String>,
}

struct BufferedRequest<'a> {
    state: &'a AppState,
    reservation: crate::model::UsageReservation,
    request_id: Uuid,
    started: Instant,
    input_token_ceiling: i64,
    output_token_ceiling: i64,
    requested_service_tier: Option<String>,
    conversation: Option<ProxyConversation>,
    protocol: Protocol,
    tenant_id: Uuid,
    memory: std::sync::Arc<crate::gateway_body::memory::ProxyMemoryReservation>,
}

#[allow(clippy::too_many_arguments)]
async fn execute_component_provider(
    request: BufferedRequest<'_>,
    driver: &str,
    base_url: &str,
    config: &Value,
    credential: &UpstreamCredential,
    prepared: PreparedProviderRequest,
    context: RequestContext,
    mut upstream_attempt: Option<UpstreamAttemptGuard>,
) -> Result<Response, AppError> {
    let target = match component_provider_url(base_url, &prepared.path) {
        Ok(target) => target,
        Err(_) => {
            tracing::warn!(request_id = %request.request_id, stage = "component_target", "component provider request failed");
            return finish_component_provider_failure(&request, "provider_unsafe_target").await;
        }
    };
    let outbound_http = match network::client_for_config_url(
        &request.state.http,
        &target,
        config,
        credential.proxy(),
        request.state.config.allow_oauth_loopback,
    )
    .await
    {
        Ok(client) => client,
        Err(_) => {
            tracing::warn!(request_id = %request.request_id, stage = "component_network_client", "component provider request failed");
            return finish_component_provider_failure(&request, "provider_unsafe_target").await;
        }
    };
    let mut upstream_request = outbound_http
        .request(prepared.method, target)
        .timeout(component_provider_timeout(config))
        .body(prepared.body);
    for (name, value) in prepared.headers {
        let name = match reqwest::header::HeaderName::from_bytes(name.as_bytes()) {
            Ok(name) => name,
            Err(_) => {
                return finish_component_provider_failure(&request, "provider_invalid_request")
                    .await;
            }
        };
        let value = match reqwest::header::HeaderValue::from_str(&value) {
            Ok(value) => value,
            Err(_) => {
                return finish_component_provider_failure(&request, "provider_invalid_request")
                    .await;
            }
        };
        upstream_request = upstream_request.header(name, value);
    }
    upstream_request = match credential.apply(upstream_request, unix_millis()) {
        Ok(request) => request,
        Err(_) => {
            tracing::warn!(request_id = %request.request_id, stage = "component_credential", "component provider request failed");
            return finish_component_provider_failure(&request, "provider_credential").await;
        }
    };
    let _upstream_activity = request
        .state
        .metrics
        .active_upstream(driver, "component_provider");
    let upstream_started = Instant::now();
    let upstream_result = upstream_request.send().await;
    request.state.metrics.observe_upstream(
        driver,
        "component_provider",
        upstream_result.as_ref().ok().map(reqwest::Response::status),
        upstream_started.elapsed(),
    );
    let upstream = match upstream_result {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(
                request_id = %request.request_id,
                is_timeout = error.is_timeout(),
                is_connect = error.is_connect(),
                "component provider upstream request failed"
            );
            if let Some(attempt) = upstream_attempt.as_mut() {
                attempt
                    .complete(UpstreamAttemptTerminal::Failed {
                        kind: crate::db::UpstreamFailureKind::Connection,
                        reason: UpstreamHealthReason::Connection,
                    })
                    .await;
            }
            return finish_component_provider_failure(&request, "upstream_connection").await;
        }
    };
    let upstream_status = upstream.status();
    if !upstream_status.is_success() && !upstream_status.is_redirection() {
        if let Some(attempt) = upstream_attempt.as_mut() {
            let terminal = if upstream_status == StatusCode::TOO_MANY_REQUESTS {
                let (response, kind) = routing::classify_rate_limit(upstream.into()).await;
                drop(response);
                UpstreamAttemptTerminal::Failed {
                    kind,
                    reason: UpstreamHealthReason::RateLimited,
                }
            } else {
                drop(upstream);
                if matches!(
                    upstream_status,
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                ) {
                    UpstreamAttemptTerminal::Failed {
                        kind: crate::db::UpstreamFailureKind::Authentication,
                        reason: UpstreamHealthReason::Unavailable,
                    }
                } else if upstream_status.is_server_error() {
                    UpstreamAttemptTerminal::Failed {
                        kind: crate::db::UpstreamFailureKind::Unavailable,
                        reason: UpstreamHealthReason::Unavailable,
                    }
                } else {
                    UpstreamAttemptTerminal::Inconclusive
                }
            };
            attempt.complete(terminal).await;
        } else {
            drop(upstream);
        }
        return finish_buffered_request(
            &request,
            upstream_status,
            Bytes::from_static(
                b"{\"error\":{\"message\":\"component provider rejected the request\"}}",
            ),
            "application/json",
            (
                TokenUsage::default(),
                crate::model::RequestUsageBasis::NotObserved,
            ),
            Some(format!("http_{}", upstream_status.as_u16())),
        )
        .await;
    }
    let mut upstream_headers = BTreeMap::new();
    for (name, value) in upstream.headers() {
        let value = match value.to_str() {
            Ok(value) => value,
            Err(_) => {
                if let Some(attempt) = upstream_attempt.as_mut() {
                    attempt
                        .complete(UpstreamAttemptTerminal::invalid_response())
                        .await;
                }
                return finish_component_provider_failure(&request, "upstream_invalid_headers")
                    .await;
            }
        };
        upstream_headers.insert(name.to_string(), value.to_owned());
    }
    let Some(maximum) = request
        .state
        .providers
        .get(driver)
        .and_then(|provider| provider.component_adapter.as_ref())
        .map(|adapter| adapter.max_response_bytes)
    else {
        return finish_component_provider_failure(&request, "provider_configuration").await;
    };
    let upstream_body = match read_bounded_upstream(
        upstream.into(),
        maximum.min(MAX_PROXY_RESPONSE_BODY),
        &request.memory,
        request.started,
        true,
    )
    .await
    {
        Ok(body) => body,
        Err(error) => {
            tracing::warn!(request_id = %request.request_id, stage = "component_response", "component provider request failed");
            if !matches!(
                error,
                buffered_upstream::BoundedUpstreamError::MemoryCapacity
            ) && let Some(attempt) = upstream_attempt.as_mut()
            {
                attempt
                    .complete(UpstreamAttemptTerminal::invalid_response())
                    .await;
            }
            return finish_component_provider_failure(&request, error.code()).await;
        }
    };
    let mut normalized = match normalize_component_provider(
        request.state,
        driver,
        context,
        upstream_status.as_u16(),
        upstream_headers,
        upstream_body,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(request_id = %request.request_id, stage = "component_normalize", "component provider request failed");
            // The adapter currently reports guest traps and invalid normalized
            // envelopes through the same typed Upstream error. Both are a
            // failed response-processing attempt, never a client cancellation.
            // Local memory/storage/runtime setup errors remain inconclusive.
            if matches!(error, AppError::Upstream(_))
                && let Some(attempt) = upstream_attempt.as_mut()
            {
                attempt
                    .complete(UpstreamAttemptTerminal::invalid_response())
                    .await;
            }
            return finish_component_provider_failure(&request, "provider_normalize").await;
        }
    };
    if !request.memory.response_capture_fits(normalized.body.len())
        || !request.memory.response_json_fits(&normalized.body)
    {
        drop(normalized);
        return finish_component_provider_failure(&request, "upstream_response_memory_capacity")
            .await;
    }
    normalized.body.shrink_to_fit();
    let status = match StatusCode::from_u16(normalized.status) {
        Ok(status) => status,
        Err(_) => {
            if let Some(attempt) = upstream_attempt.as_mut() {
                attempt
                    .complete(UpstreamAttemptTerminal::invalid_response())
                    .await;
            }
            return finish_component_provider_failure(&request, "provider_invalid_response").await;
        }
    };
    if !status.is_success() {
        if let Some(attempt) = upstream_attempt.as_mut() {
            attempt
                .complete(UpstreamAttemptTerminal::invalid_response())
                .await;
        }
        return finish_buffered_request(
            &request,
            status,
            Bytes::from_static(
                b"{\"error\":{\"message\":\"component provider rejected the request\"}}",
            ),
            "application/json",
            (
                TokenUsage::default(),
                crate::model::RequestUsageBasis::NotObserved,
            ),
            Some(format!("http_{}", status.as_u16())),
        )
        .await;
    }
    let input_tokens = i64::try_from(normalized.input_tokens).ok();
    let output_tokens = i64::try_from(normalized.output_tokens).ok();
    let usage_is_valid = input_tokens.is_some_and(|tokens| {
        (0..=MAX_REPORTED_TOKENS).contains(&tokens) && tokens <= request.input_token_ceiling
    }) && output_tokens.is_some_and(|tokens| {
        (0..=MAX_REPORTED_TOKENS).contains(&tokens) && tokens <= request.output_token_ceiling
    });
    if !usage_is_valid {
        if let Some(attempt) = upstream_attempt.as_mut() {
            attempt
                .complete(UpstreamAttemptTerminal::invalid_response())
                .await;
        }
        return finish_component_provider_failure(&request, "upstream_invalid_usage").await;
    }
    if normalized.estimated {
        tracing::debug!(request_id = %request.request_id, stage = "component_usage", "component provider reported estimated usage");
    }
    let usage = if status.is_success() {
        (
            TokenUsage {
                input_tokens: input_tokens.unwrap_or_default(),
                output_tokens: output_tokens.unwrap_or_default(),
                ..TokenUsage::default()
            },
            if normalized.estimated {
                crate::model::RequestUsageBasis::ProviderEstimated
            } else {
                crate::model::RequestUsageBasis::ProviderReported
            },
        )
    } else {
        (
            TokenUsage::default(),
            crate::model::RequestUsageBasis::NotObserved,
        )
    };
    let content_type = normalized
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.as_str())
        .unwrap_or("application/json");
    let result = finish_buffered_request(
        &request,
        status,
        Bytes::from(normalized.body),
        content_type,
        usage,
        None,
    )
    .await;
    // A component's 2xx headers or normalized output do not prove recovery.
    // Only a successful durable terminal settlement may heal the probe.
    if let Some(attempt) = upstream_attempt.as_mut()
        && result
            .as_ref()
            .is_ok_and(|response| response.status().is_success())
    {
        attempt.complete(UpstreamAttemptTerminal::Succeeded).await;
    }
    result
}

async fn finish_component_provider_failure(
    request: &BufferedRequest<'_>,
    error_code: &str,
) -> Result<Response, AppError> {
    let memory_capacity = error_code == "upstream_response_memory_capacity";
    if memory_capacity {
        request
            .state
            .metrics
            .record_proxy_memory_rejection(crate::metrics::ProxyMemoryRejectionStage::Response);
    }
    finish_buffered_request(
        request,
        StatusCode::BAD_GATEWAY,
        Bytes::from_static(b"{\"error\":{\"message\":\"component provider request failed\"}}"),
        "application/json",
        (
            TokenUsage::default(),
            crate::model::RequestUsageBasis::NotObserved,
        ),
        Some(error_code.to_owned()),
    )
    .await
}

async fn finish_proxy_failure(
    request: &BufferedRequest<'_>,
    error_code: &str,
) -> Result<Response, AppError> {
    let memory_capacity = error_code == "upstream_response_memory_capacity";
    if memory_capacity {
        request
            .state
            .metrics
            .record_proxy_memory_rejection(crate::metrics::ProxyMemoryRejectionStage::Response);
    }
    finish_buffered_request(
        request,
        StatusCode::BAD_GATEWAY,
        Bytes::from_static(
            b"{\"error\":{\"message\":\"upstream request failed\",\"type\":\"upstream_error\"}}",
        ),
        "application/json",
        (
            TokenUsage::default(),
            crate::model::RequestUsageBasis::NotObserved,
        ),
        Some(error_code.to_owned()),
    )
    .await
}

async fn finish_buffered_request(
    request: &BufferedRequest<'_>,
    status: StatusCode,
    body: Bytes,
    content_type: &str,
    usage: (TokenUsage, crate::model::RequestUsageBasis),
    error_code: Option<String>,
) -> Result<Response, AppError> {
    finish_buffered_request_with_upstream_attribution(
        request,
        status,
        body,
        content_type,
        usage,
        error_code,
        ProxyRequestUpstreamAttribution::KeepSelected,
    )
    .await
}

async fn finish_buffered_request_with_upstream_attribution(
    request: &BufferedRequest<'_>,
    mut status: StatusCode,
    mut body: Bytes,
    content_type: &str,
    usage: (TokenUsage, crate::model::RequestUsageBasis),
    mut error_code: Option<String>,
    upstream_attribution: ProxyRequestUpstreamAttribution,
) -> Result<Response, AppError> {
    let request_id = request.request_id;
    let (usage, mut usage_basis) = usage;
    let usage = match crate::db::normalize_proxy_usage(
        &usage,
        request.input_token_ceiling,
        request.output_token_ceiling,
        request.requested_service_tier.as_deref(),
    ) {
        Ok(usage) => usage,
        Err(AppError::Upstream(_)) => {
            status = StatusCode::BAD_GATEWAY;
            body = Bytes::from_static(
                b"{\"error\":{\"message\":\"upstream returned invalid usage\",\"type\":\"upstream_error\"}}",
            );
            error_code = Some("upstream_invalid_usage".to_owned());
            usage_basis = crate::model::RequestUsageBasis::NotObserved;
            TokenUsage::default()
        }
        Err(error) => return Err(error),
    };
    let _response_buffer = request
        .state
        .metrics
        .memory_usage(crate::metrics::MemoryComponent::ResponseBuffer, body.len());
    let response_id = (status.is_success()
        && error_code.is_none()
        && matches!(request.protocol, Protocol::OpenAiResponses))
    .then(|| extract_response_id(&body))
    .flatten();
    // Seal the independent response spool in the terminal transaction. Only
    // its durable ACK gates delivery, never an object-store upload.
    let capture_started = Instant::now();
    let response_capture_memory = request.state.metrics.memory_usage(
        crate::metrics::MemoryComponent::StreamCapture,
        body.len().saturating_mul(3),
    );
    let response_capture_permit = request.state.proxy_memory_budget.reservation();
    let response_archive = if request.memory.has_buffered_response()
        || response_capture_permit.try_grow(
            body.len(),
            crate::gateway_body::memory::CAPTURE_MEMORY_WEIGHT,
        ) {
        BufferedArchive::new(
            crate::db::ArchiveSpoolIdentity {
                request_id,
                tenant_id: request.tenant_id,
                reservation_id: request.reservation.id,
            },
            crate::response_archive_spool::BufferedArchivePurpose::Response,
            &body,
            request.state.config.key_pepper.as_bytes(),
            request.state.config.archive_spool_compression_enabled,
        )
    } else {
        Err(AppError::Overloaded)
    };
    let stored_response = format!("gap://{request_id}/response");
    let conversation = request
        .conversation
        .as_ref()
        .map(|conversation| ProxyConversationInput {
            key: &conversation.key,
            request_json: &conversation.request_json,
            hints: &conversation.hints,
            client_name: conversation.client_name.as_deref(),
            upstream_response_id: response_id.as_deref(),
        });
    let terminal = FinishProxyRequest {
        first_output_ms: None,
        generation_duration_ms: None,
        request_id,
        tenant_id: request.tenant_id,
        reservation: &request.reservation,
        input_token_ceiling: request.input_token_ceiling,
        output_token_ceiling: request.output_token_ceiling,
        requested_service_tier: request.requested_service_tier.as_deref(),
        status_code: i64::from(status.as_u16()),
        duration_ms: request.started.elapsed().as_millis() as i64,
        usage,
        usage_basis: Some(usage_basis),
        charge_contract_ceiling: false,
        error_code: error_code.as_deref(),
        response_object: &stored_response,
        conversation,
    };
    let terminal_phase = proxy_diagnostics::Phase::new(
        proxy_diagnostics::Context::for_request(request_id),
        "buffered_archive_settlement",
    );
    let result = match response_archive {
        Ok(archive) => {
            lifecycle::finish_buffered_proxy_request_with_retry(
                &request.state.db,
                terminal,
                &archive,
                upstream_attribution,
            )
            .await
        }
        Err(_) => {
            tracing::warn!(
                phase = "response_encrypt",
                error_code = "capture_failed",
                elapsed_ms = capture_started.elapsed().as_millis() as u64,
                "proxy archive gap"
            );
            finish_proxy_request_with_retry(&request.state.db, terminal, None, upstream_attribution)
                .await
        }
    };
    terminal_phase.finish(
        if result.is_ok() {
            "completed"
        } else {
            "failed"
        },
        Some(status.as_u16()),
        Some(body.len()),
    );
    drop(response_capture_memory);
    drop(response_capture_permit);
    if result.is_err() {
        tracing::error!(%request_id, stage = "buffered_terminal_transaction", "proxy request finalization failed");
    }
    let result = result?;
    if matches!(result, FinishProxyRequestResult::AlreadyFinished { .. }) {
        tracing::debug!(%request_id, stage = "terminal_replay", "proxy request already finalized");
    }
    Response::builder()
        .status(status)
        .header(REQUEST_ID_HEADER, request_id.to_string())
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .map_err(|_| AppError::Internal)
}

fn routing_selection_seed(
    key: &AuthenticatedKey,
    request_id: Uuid,
    hints: &crate::conversation::ConversationHints,
) -> Uuid {
    let Some(session_id) = hints.session_id.as_deref() else {
        return request_id;
    };
    let mut hasher = blake3::Hasher::new_derive_key("memeloop routing session affinity v1");
    hasher.update(key.tenant_id.as_bytes());
    hasher.update(key.key_id.as_bytes());
    hasher.update(session_id.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    Uuid::from_bytes(bytes)
}
