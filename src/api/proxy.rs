use super::*;

#[path = "codex_transport.rs"]
mod codex_transport;

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
use crate::{
    db::{SwitchProxyCandidateInput, UpstreamAttemptAdmission},
    metrics::{UpstreamHealthEvent, UpstreamHealthReason},
    provider::AuthorizedUpstreamCandidate,
};
use chat_sse_usage::ChatSseUsageContract;
pub(in crate::api) use conversation_hints::safe_conversation_hint as safe_response_id;
use conversation_hints::{client_name, conversation_hints};
use lifecycle::{
    finish_proxy_request_with_archive_fallback, run_bounded_proxy_lifecycle,
    run_bounded_text_archive,
};
use routing::{
    AdmittedProxyRouteInput, CandidatePreparationSummary, CodexRetryTerminal,
    CodexRetryTerminalGuard, DeferredSharedProbe, NextSendableProxyRouteInput,
    PROXY_ROUTING_POLICY, PlannedProxyRoute, PreparedProxyRoute, PreparedRouteReadiness,
    ProxyRequestContext, ProxyRoutePlanInput, ProxySendError, UpstreamAttemptGuard,
    UpstreamAttemptTerminal, candidate_reservation_bounds, exhausted_candidate_error,
    materialize_proxy_route, next_planned_proxy_candidate, plan_proxy_route,
    prepare_admitted_proxy_route, refresh_route_snapshot, send_proxy_route,
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
        deferred_shared_probes,
    } = input;
    let state = request.state;
    let request_id = request.request_id;
    let outbound_attempt = outbound_attempts.saturating_add(1);
    let strict_choice_count_is_incompatible = matches!(request.protocol, Protocol::OpenAiChat)
        && openai_chat_choice_count(request.request_json)? != 1;
    let mut summary = CandidatePreparationSummary::default();
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
        let admission = state
            .db
            .claim_upstream_account_attempt_with_health_config(
                planned.route.account_id,
                planned.route.credential_generation,
                state.config.upstream_health,
            )
            .await?;
        if let UpstreamAttemptAdmission::Unavailable {
            cooldown_until,
            probe_lease_until,
            shared_probe_eligible,
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
        );
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
    let response_body = match read_bounded_upstream(upstream, MAX_PROXY_RESPONSE_BODY).await {
        Ok(body) => Bytes::from(body),
        Err(error) => {
            let result = finish_proxy_failure(buffered_request, error.code()).await;
            upstream_attempt
                .complete(UpstreamAttemptTerminal::invalid_response())
                .await;
            return result;
        }
    };
    if matches!(protocol, Protocol::OpenAiResponses)
        && let Err(error_code) = validate_buffered_responses_success(&response_body)
    {
        let result = finish_proxy_failure(buffered_request, error_code).await;
        upstream_attempt
            .complete(UpstreamAttemptTerminal::invalid_response())
            .await;
        return result;
    }
    let usage = if capture_json_usage {
        match extract_usage_checked(&response_body) {
            ExtractedUsage::Valid(usage) => usage,
            ExtractedUsage::Missing => TokenUsage {
                input_tokens: input_token_ceiling,
                output_tokens: output_token_ceiling,
                ..TokenUsage::default()
            },
            ExtractedUsage::Invalid => {
                let result = finish_proxy_failure(buffered_request, "upstream_invalid_usage").await;
                upstream_attempt
                    .complete(UpstreamAttemptTerminal::invalid_response())
                    .await;
                return result;
            }
        }
    } else {
        TokenUsage {
            input_tokens: input_token_ceiling,
            output_tokens: output_token_ceiling,
            ..TokenUsage::default()
        }
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
    price: &ModelPrice,
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

pub(super) async fn proxy(
    state: AppState,
    headers: HeaderMap,
    body: Bytes,
    protocol: Protocol,
) -> Result<Response, AppError> {
    let _request_buffer = state
        .metrics
        .memory_usage(crate::metrics::MemoryComponent::RequestBuffer, body.len());
    let key = authenticate_downstream(&headers, &state).await?;
    let proxy_lifecycle_permit = state
        .proxy_lifecycle_permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| AppError::Overloaded)?;
    let request_id = Uuid::now_v7();
    let original_request_json: Value = serde_json::from_slice(&body)
        .map_err(|_| AppError::BadRequest("request body must be valid JSON".into()))?;
    let conversation_hints = conversation_hints(&headers, &original_request_json);
    let applied = apply_traffic_policy(
        &state,
        &key,
        TrafficPolicyProtocols::same(protocol.name()),
        original_request_json.clone(),
    )
    .await?;
    let request_json = applied.request_json;
    let model = applied.model;
    let selection_seed = routing_selection_seed(&key, request_id, &conversation_hints);
    let candidates = state
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
    let request_context = ProxyRequestContext {
        state: &state,
        key: &key,
        model: &model,
        protocol,
        request_id,
        request_json: &request_json,
    };
    let route_plan = prepare_authorized_proxy_routes(AuthorizedProxyRoutesInput {
        request: request_context,
        original_body_length: body.len(),
        candidates,
    })
    .await?;
    let primary = route_plan.primary_route();
    let upstream_account_id = Some(primary.account_id);
    let model_route_id = Some(primary.route_id);
    let price = state.db.model_price(&model, &key.currency).await?;
    let input_token_ceiling = route_plan.input_token_ceiling;
    let output_token_ceiling = route_plan.output_token_ceiling;
    let requested_service_tier = requested_service_tier(&request_json, &price)?;
    let request_digest = blake3::hash(&body).to_hex();
    let admitted_request_object = format!("gap://{request_id}/request");
    let request_archive_attempt =
        match begin_proxy_archive_attempt(&state.db, request_id, ArchiveStagingPurpose::Request)
            .await
        {
            Ok(attempt) => Some(attempt),
            Err(_) => {
                tracing::warn!(%request_id, stage = "request_archive_begin", "proxy archive gap");
                None
            }
        };
    let reservation = match state
        .db
        .start_proxy_request(StartProxyRequest {
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
        })
        .await
    {
        Ok(reservation) => reservation,
        Err(error) => {
            tracing::error!(%request_id, stage = "request_transaction_admission", "proxy request admission failed");
            if let Some(attempt) = request_archive_attempt.as_ref() {
                abandon_proxy_archive_attempt(&state.db, attempt).await;
            }
            return Err(error);
        }
    };
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

    let started = Instant::now();
    let mut buffered_request = BufferedRequest {
        state: &state,
        reservation,
        request_id,
        started,
        input_token_ceiling,
        output_token_ceiling,
        requested_service_tier,
        conversation,
        protocol,
        tenant_id: key.tenant_id,
        archive_available: false,
    };
    if let Some(attempt) = request_archive_attempt.as_ref() {
        let archive = async {
            let mut writer = state.archive.start_writer(&attempt.object_locator).await?;
            writer.write(body.clone()).await?;
            let staged = writer.finish_staged().await?;
            if staged.blake3_digest != request_digest.as_str()
                || staged.object_locator != attempt.object_locator
            {
                return Err(AppError::Storage(
                    "proxy request archive verification failed".into(),
                ));
            }
            attach_proxy_archive_with_retry(
                &state.db,
                request_id,
                key.tenant_id,
                buffered_request.reservation.id,
                &admitted_request_object,
                attempt,
            )
            .await?;
            Ok::<(), AppError>(())
        };
        match run_bounded_text_archive(archive).await {
            Ok(Ok(())) => buffered_request.archive_available = true,
            Ok(Err(_)) | Err(_) => {
                // This is safe even after an unknown attach acknowledgement:
                // a committed bind is no longer in the writable state, so the
                // abandon CAS becomes a no-op instead of deleting owned data.
                abandon_proxy_archive_attempt(&state.db, attempt).await;
                tracing::warn!(%request_id, stage = "request_archive", "proxy archive gap");
            }
        }
    }
    let AuthorizedProxyRoutes {
        mut primary,
        remaining_candidates,
        input_token_ceiling: _,
        output_token_ceiling: _,
        output_choice_count,
    } = route_plan;
    if primary.is_component() {
        let readiness = match refresh_route_snapshot(&state, &mut primary.route).await {
            Ok(readiness) => readiness,
            Err(error) => {
                tracing::warn!(
                    %request_id,
                    upstream_account_id = %primary.route.account_id,
                    error = %error,
                    "current upstream credential is invalid"
                );
                return finish_proxy_failure(&buffered_request, "upstream_credential_invalid")
                    .await;
            }
        };
        if readiness != PreparedRouteReadiness::Ready {
            return finish_proxy_unavailable(&buffered_request, readiness.error_code()).await;
        }
        let mut active_route = match materialize_proxy_route(&state, primary).await {
            Ok(prepared) => prepared,
            Err(_) => {
                return finish_proxy_failure(&buffered_request, "provider_candidate_invalid").await;
            }
        };
        let Some((prepared, component_context)) = active_route.component_request.take() else {
            return finish_proxy_failure(&buffered_request, "provider_candidate_invalid").await;
        };
        return execute_component_provider(
            buffered_request,
            &active_route.route.driver,
            &active_route.route.base_url,
            &active_route.route.config,
            &active_route.route.credential,
            prepared,
            component_context,
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
    let mut candidate_rank = 0_usize;
    let mut deferred_shared_probes = std::collections::VecDeque::new();
    let mut next_failover_reason = None;
    let (active_route, upstream, upstream_activity, mut codex_retry, mut upstream_attempt) = loop {
        if outbound_attempts == PROXY_ROUTING_POLICY.max_attempts() {
            return finish_proxy_unavailable(&buffered_request, "upstream_attempts_exhausted")
                .await;
        }
        let selected = match next_sendable_proxy_route(NextSendableProxyRouteInput {
            request: request_context,
            price: &price,
            reservation: &mut buffered_request.reservation,
            input_token_ceiling: &mut buffered_request.input_token_ceiling,
            output_token_ceiling: &mut buffered_request.output_token_ceiling,
            original_body_length: body.len(),
            output_choice_count,
            assigned_route: &mut assigned_route,
            planned_candidate: &mut planned_candidate,
            candidates: &mut route_candidates,
            failover_reason: next_failover_reason.take(),
            candidate_rank: &mut candidate_rank,
            outbound_attempts,
            deferred_shared_probes: &mut deferred_shared_probes,
        })
        .await
        {
            Ok(selected) => selected,
            Err(error) => {
                tracing::warn!(
                    %request_id,
                    error = %error,
                    "proxy candidate selection failed"
                );
                return finish_proxy_failure(&buffered_request, "upstream_candidate_invalid").await;
            }
        };
        let Some((active_route, mut upstream_attempt, selected_candidate_rank, outbound_attempt)) =
            selected
        else {
            return finish_proxy_unavailable(&buffered_request, "upstream_unavailable").await;
        };
        let (result, rate_limit) = match send_proxy_route(
            &state,
            &headers,
            protocol,
            request_id,
            &active_route,
            selected_candidate_rank,
            outbound_attempt,
        )
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
        let consumed_outbound_attempt = !matches!(
            &result,
            Err(ProxySendError::CandidateUnavailable | ProxySendError::CredentialUnavailable)
        );
        if consumed_outbound_attempt {
            outbound_attempts += 1;
        }
        let failure = routing::classify_attempt_failure(&result, rate_limit);
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
        let failover_reason = match &result {
            Ok(result)
                if result.response.status() == StatusCode::TOO_MANY_REQUESTS
                    && (!route_candidates.as_slice().is_empty()
                        || !deferred_shared_probes.is_empty()) =>
            {
                Some(UpstreamHealthReason::RateLimited)
            }
            Err(ProxySendError::RetryableConnection(_)) => failure.map(|(_, reason)| reason),
            Err(ProxySendError::CandidateUnavailable | ProxySendError::CredentialUnavailable) => {
                Some(UpstreamHealthReason::Unavailable)
            }
            Ok(_)
            | Err(
                ProxySendError::RetryableCodexBadRequest
                | ProxySendError::CodexBadRequest
                | ProxySendError::AmbiguousResponse(_)
                | ProxySendError::NonRetryableTransport
                | ProxySendError::Credential,
            ) => None,
        };
        if let Some(reason) = failover_reason
            && (!consumed_outbound_attempt
                || outbound_attempts < PROXY_ROUTING_POLICY.max_attempts())
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
                return finish_proxy_unavailable(
                    &buffered_request,
                    "upstream_credential_unavailable",
                )
                .await;
            }
            Err(ProxySendError::RetryableConnection(_) | ProxySendError::CandidateUnavailable) => {
                return finish_proxy_unavailable(&buffered_request, "upstream_connection").await;
            }
            Err(ProxySendError::RetryableCodexBadRequest) => {
                return finish_proxy_unavailable(&buffered_request, "upstream_rejected").await;
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
                    TokenUsage::default(),
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
            Err(ProxySendError::NonRetryableTransport) => {
                upstream_attempt
                    .complete(UpstreamAttemptTerminal::Inconclusive)
                    .await;
                return finish_proxy_failure(&buffered_request, "upstream_transport").await;
            }
        }
    };
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
            TokenUsage::default(),
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
        let buffered = match codex_transport::buffer_response(upstream).await {
            Ok(buffered) => buffered,
            Err(error_code) => {
                tracing::warn!(%request_id, stage = error_code, "Codex upstream response failed");
                let result = finish_proxy_failure(&buffered_request, error_code).await;
                upstream_attempt
                    .complete(UpstreamAttemptTerminal::invalid_response())
                    .await;
                codex_retry.complete(CodexRetryTerminal::Failed);
                return result;
            }
        };
        let result = finish_buffered_request(
            &buffered_request,
            StatusCode::OK,
            buffered.body,
            "application/json",
            buffered.usage,
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
    let strict_openai_chat_usage = requires_strict_openai_chat_usage(
        protocol,
        &active_route.route.driver,
        &active_route.route.config,
        &request_json,
    );
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
        upstream_attempt,
        upstream_activity,
        request_id,
        upstream_account_id: active_route.route.account_id,
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
    archive_available: bool,
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
            return finish_component_provider_failure(&request, "upstream_connection").await;
        }
    };
    let upstream_status = upstream.status();
    if !upstream_status.is_success() && !upstream_status.is_redirection() {
        drop(upstream);
        return finish_buffered_request(
            &request,
            upstream_status,
            Bytes::from_static(
                b"{\"error\":{\"message\":\"component provider rejected the request\"}}",
            ),
            "application/json",
            TokenUsage::default(),
            Some(format!("http_{}", upstream_status.as_u16())),
        )
        .await;
    }
    let mut upstream_headers = BTreeMap::new();
    for (name, value) in upstream.headers() {
        let value = match value.to_str() {
            Ok(value) => value,
            Err(_) => {
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
    let upstream_body = match read_bounded_upstream(upstream.into(), maximum).await {
        Ok(body) => body,
        Err(error) => {
            tracing::warn!(request_id = %request.request_id, stage = "component_response", "component provider request failed");
            return finish_component_provider_failure(&request, error.code()).await;
        }
    };
    let normalized = match normalize_component_provider(
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
        Err(_) => {
            tracing::warn!(request_id = %request.request_id, stage = "component_normalize", "component provider request failed");
            return finish_component_provider_failure(&request, "provider_normalize").await;
        }
    };
    let status = match StatusCode::from_u16(normalized.status) {
        Ok(status) => status,
        Err(_) => {
            return finish_component_provider_failure(&request, "provider_invalid_response").await;
        }
    };
    if !status.is_success() {
        return finish_buffered_request(
            &request,
            status,
            Bytes::from_static(
                b"{\"error\":{\"message\":\"component provider rejected the request\"}}",
            ),
            "application/json",
            TokenUsage::default(),
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
        return finish_component_provider_failure(&request, "upstream_invalid_usage").await;
    }
    if normalized.estimated {
        tracing::debug!(request_id = %request.request_id, stage = "component_usage", "component provider reported estimated usage");
    }
    let usage = if status.is_success() {
        TokenUsage {
            input_tokens: input_tokens.unwrap_or_default(),
            output_tokens: output_tokens.unwrap_or_default(),
            ..TokenUsage::default()
        }
    } else {
        TokenUsage::default()
    };
    let content_type = normalized
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.as_str())
        .unwrap_or("application/json");
    finish_buffered_request(
        &request,
        status,
        Bytes::from(normalized.body),
        content_type,
        usage,
        None,
    )
    .await
}

async fn finish_component_provider_failure(
    request: &BufferedRequest<'_>,
    error_code: &str,
) -> Result<Response, AppError> {
    finish_buffered_request(
        request,
        StatusCode::BAD_GATEWAY,
        Bytes::from_static(b"{\"error\":{\"message\":\"component provider request failed\"}}"),
        "application/json",
        TokenUsage::default(),
        Some(error_code.to_owned()),
    )
    .await
}

async fn finish_proxy_failure(
    request: &BufferedRequest<'_>,
    error_code: &str,
) -> Result<Response, AppError> {
    finish_buffered_request(
        request,
        StatusCode::BAD_GATEWAY,
        Bytes::from_static(
            b"{\"error\":{\"message\":\"upstream request failed\",\"type\":\"upstream_error\"}}",
        ),
        "application/json",
        TokenUsage::default(),
        Some(error_code.to_owned()),
    )
    .await
}

async fn finish_proxy_unavailable(
    request: &BufferedRequest<'_>,
    error_code: &str,
) -> Result<Response, AppError> {
    let mut response = finish_buffered_request(
        request,
        StatusCode::SERVICE_UNAVAILABLE,
        Bytes::from_static(
            b"{\"error\":{\"message\":\"no healthy upstream is currently available\",\"type\":\"upstream_error\"}}",
        ),
        "application/json",
        TokenUsage::default(),
        Some(error_code.to_owned()),
    )
    .await?;
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    Ok(response)
}

async fn finish_buffered_request(
    request: &BufferedRequest<'_>,
    mut status: StatusCode,
    mut body: Bytes,
    content_type: &str,
    usage: TokenUsage,
    mut error_code: Option<String>,
) -> Result<Response, AppError> {
    let request_id = request.request_id;
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
    let mut response_archive_attempt = if request.archive_available {
        match begin_proxy_archive_attempt(
            &request.state.db,
            request_id,
            ArchiveStagingPurpose::Response,
        )
        .await
        {
            Ok(attempt) => Some(attempt),
            Err(_) => {
                tracing::warn!(%request_id, stage = "buffered_response_archive_begin", "proxy archive gap");
                None
            }
        }
    } else {
        None
    };
    let stored_response = if let Some(attempt) = response_archive_attempt.as_ref() {
        let archive = async {
            let mut writer = request
                .state
                .archive
                .start_writer(&attempt.object_locator)
                .await?;
            writer.write(body.clone()).await?;
            let staged = writer.finish_staged().await?;
            if staged.object_locator != attempt.object_locator {
                return Err(AppError::Storage(
                    "proxy response archive verification failed".into(),
                ));
            }
            Ok::<String, AppError>(staged.object_locator)
        };
        match run_bounded_text_archive(archive).await {
            Ok(Ok(stored)) => stored,
            Ok(Err(_)) | Err(_) => {
                abandon_proxy_archive_attempt(&request.state.db, attempt).await;
                response_archive_attempt = None;
                tracing::warn!(%request_id, stage = "buffered_response_archive", "proxy archive gap");
                format!("gap://{request_id}/response")
            }
        }
    } else {
        format!("gap://{request_id}/response")
    };
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
    let gap_response = format!("gap://{request_id}/response");
    let result = finish_proxy_request_with_archive_fallback(
        &request.state.db,
        FinishProxyRequest {
            request_id,
            tenant_id: request.tenant_id,
            reservation: &request.reservation,
            input_token_ceiling: request.input_token_ceiling,
            output_token_ceiling: request.output_token_ceiling,
            requested_service_tier: request.requested_service_tier.as_deref(),
            status_code: i64::from(status.as_u16()),
            duration_ms: request.started.elapsed().as_millis() as i64,
            usage,
            charge_contract_ceiling: false,
            error_code: error_code.as_deref(),
            response_object: &stored_response,
            conversation,
        },
        response_archive_attempt.as_ref(),
        &gap_response,
    )
    .await;
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

#[derive(Clone, Copy)]
enum BoundedUpstreamError {
    ResponseTooLarge,
    Stream,
}

impl BoundedUpstreamError {
    fn code(self) -> &'static str {
        match self {
            Self::ResponseTooLarge => "upstream_response_too_large",
            Self::Stream => "upstream_stream",
        }
    }
}

async fn read_bounded_upstream(
    response: UpstreamResponse,
    maximum: usize,
) -> Result<Vec<u8>, BoundedUpstreamError> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(BoundedUpstreamError::ResponseTooLarge);
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        // Never retain or display reqwest's error: its URL can contain
        // credential-bearing upstream configuration.
        let chunk = chunk.map_err(|_| BoundedUpstreamError::Stream)?;
        if body.len().saturating_add(chunk.len()) > maximum {
            return Err(BoundedUpstreamError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
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
