use super::*;

#[path = "codex_transport.rs"]
mod codex_transport;

mod conversation_hints;
mod lifecycle;
mod routing;
mod streaming;

use crate::{
    db::{UpstreamAttemptAdmission, UpstreamFailureKind},
    metrics::{UpstreamHealthEvent, UpstreamHealthReason},
};
use conversation_hints::{client_name, conversation_hints, safe_conversation_hint};
use lifecycle::{
    AbortTaskOnDrop, begin_streaming_response_archive,
    finish_proxy_request_with_archive_fallback, run_bounded_proxy_lifecycle,
    run_bounded_text_archive,
};
use routing::{
    MAX_UPSTREAM_ATTEMPTS, ProxySendError, prepare_proxy_route, retryable_upstream_status,
    send_proxy_route,
};

#[cfg(test)]
mod tests;

const PROXY_BODY_CHANNEL_CAPACITY: usize = 1;
const MAX_INPUT_TOKEN_OVERHEAD_CEILING: i64 = 1_000_000;

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
    let resolved_routes = state
        .db
        .resolve_authorized_upstream_candidates_with_hint(
            key.key_id,
            key.tenant_id,
            &model,
            protocol.name(),
            RouteSelectionOptions {
                upstream_account_hint: applied.upstream_account_hint,
                selection_seed,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await?;
    let mut resolved_routes = resolved_routes.into_iter();
    let Some(primary_route) = resolved_routes.next() else {
        // Normalized grants are the sole downstream authorization source.
        // A missing route must never fall back to process-wide legacy secrets.
        return Err(AppError::Forbidden);
    };
    let primary =
        prepare_proxy_route(&state, &key, &model, protocol, &request_json, primary_route).await?;
    let primary_is_component = primary.is_component(&state);
    let mut prepared_routes = vec![primary];
    if !primary_is_component {
        for route in resolved_routes {
            if prepared_routes.len() == MAX_UPSTREAM_ATTEMPTS {
                break;
            }
            if state
                .providers
                .get(&route.driver)
                .and_then(|provider| provider.component_adapter.as_ref())
                .is_some()
            {
                // Plugin prepare may perform HTTP/KV side effects and component
                // requests may use arbitrary methods. A standby component route
                // must be rejected before its prepare hook sees the request.
                continue;
            }
            match prepare_proxy_route(&state, &key, &model, protocol, &request_json, route).await {
                Ok(prepared) => prepared_routes.push(prepared),
                Err(error) => {
                    tracing::warn!(%request_id, error = %error, stage = "candidate_prepare", "proxy failover candidate is unusable");
                }
            }
        }
    }
    let primary = prepared_routes.first().ok_or(AppError::Internal)?;
    let upstream_account_id = Some(primary.route.account_id);
    let model_route_id = Some(primary.route.route_id);
    let price = state.db.model_price(&model, &key.currency).await?;
    let mut input_token_ceiling = 0_i64;
    let mut output_token_ceiling = 0_i64;
    for route in &prepared_routes {
        let body_ceiling =
            i64::try_from(route.request_body_ceiling(body.len())).unwrap_or(i64::MAX);
        let candidate_ceiling = body_ceiling
            .checked_add(trusted_input_token_overhead_ceiling(
                Some(&route.route.driver),
                Some(&route.route.config),
            )?)
            .filter(|ceiling| *ceiling <= MAX_REPORTED_TOKENS)
            .ok_or_else(|| {
                AppError::Upstream(
                    "upstream input token reservation is outside the supported range".into(),
                )
            })?;
        input_token_ceiling = input_token_ceiling.max(candidate_ceiling);
        output_token_ceiling = output_token_ceiling.max(route.output_token_ceiling);
    }
    let requested_service_tier = match request_json.get("service_tier") {
        None => None,
        Some(Value::String(tier)) if is_supported_service_tier(tier) => Some(tier.clone()),
        Some(_) => {
            return Err(AppError::BadRequest(
                "service_tier must be default, auto, priority, flex, scale, batch, or standard_only"
                    .into(),
            ));
        }
    };
    if let Some(tier) = requested_service_tier.as_deref()
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
    let request_digest = blake3::hash(&body).to_hex();
    let admitted_request_object = format!("gap://{request_id}/request");
    let request_archive_attempt = match begin_proxy_archive_attempt(
        &state.db,
        request_id,
        ArchiveStagingPurpose::Request,
    )
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
    let mut route_attempts = prepared_routes.into_iter();
    let mut active_route = route_attempts.next().ok_or(AppError::Internal)?;
    if let Some((prepared, component_context)) = active_route.component_request.take() {
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
    let (upstream, upstream_activity) = loop {
        let admission = state
            .db
            .claim_upstream_account_attempt(active_route.route.account_id)
            .await?;
        if admission == UpstreamAttemptAdmission::Unavailable {
            state.metrics.observe_upstream_health(
                UpstreamHealthEvent::Skipped,
                UpstreamHealthReason::Cooldown,
            );
            let Some(next_route) = route_attempts.next() else {
                return finish_proxy_unavailable(&buffered_request, "upstream_cooldown").await;
            };
            if state
                .db
                .reassign_pending_proxy_upstream(
                    request_id,
                    key.tenant_id,
                    buffered_request.reservation.id,
                    (active_route.route.account_id, active_route.route.route_id),
                    (next_route.route.account_id, next_route.route.route_id),
                )
                .await
                .is_err()
            {
                return finish_proxy_failure(&buffered_request, "upstream_failover_state").await;
            }
            state.metrics.observe_upstream_health(
                UpstreamHealthEvent::Failover,
                UpstreamHealthReason::Cooldown,
            );
            active_route = next_route;
            continue;
        }
        let result = send_proxy_route(&state, &headers, protocol, request_id, &active_route).await;
        let failure = match &result {
            Ok((response, _)) if response.status() == StatusCode::TOO_MANY_REQUESTS => Some((
                UpstreamFailureKind::RateLimited,
                UpstreamHealthReason::RateLimited,
            )),
            Ok((response, _)) if retryable_upstream_status(response.status()) => Some((
                UpstreamFailureKind::Unavailable,
                UpstreamHealthReason::Unavailable,
            )),
            Ok((response, _))
                if active_route.is_codex()
                    && response.status().is_success()
                    && !codex_transport::is_event_stream(response) =>
            {
                Some((
                    UpstreamFailureKind::InvalidResponse,
                    UpstreamHealthReason::InvalidResponse,
                ))
            }
            Err(ProxySendError::RetryableConnection | ProxySendError::CandidateUnavailable) => {
                Some((
                    UpstreamFailureKind::Connection,
                    UpstreamHealthReason::Connection,
                ))
            }
            Ok(_) => None,
            Err(ProxySendError::NonRetryableTransport | ProxySendError::Credential) => None,
        };
        if let Some((kind, reason)) = failure {
            if let Err(error) = state
                .db
                .record_upstream_account_failure(active_route.route.account_id, kind)
                .await
            {
                tracing::warn!(
                    %request_id,
                    upstream_account_id = %active_route.route.account_id,
                    error = %error,
                    "failed to persist upstream account cooldown"
                );
            }
            state
                .metrics
                .observe_upstream_health(UpstreamHealthEvent::Failure, reason);
        }
        if let Some((_, reason)) = failure
            && let Some(next_route) = route_attempts.next()
        {
            if let Ok((response, _activity)) = result {
                drop(response);
            }
            if state
                .db
                .reassign_pending_proxy_upstream(
                    request_id,
                    key.tenant_id,
                    buffered_request.reservation.id,
                    (active_route.route.account_id, active_route.route.route_id),
                    (next_route.route.account_id, next_route.route.route_id),
                )
                .await
                .is_err()
            {
                return finish_proxy_failure(&buffered_request, "upstream_failover_state").await;
            }
            tracing::warn!(
                %request_id,
                failed_upstream_account_id = %active_route.route.account_id,
                next_upstream_account_id = %next_route.route.account_id,
                stage = "upstream_failover",
                "proxy is switching to the next authorized upstream before downstream delivery"
            );
            state
                .metrics
                .observe_upstream_health(UpstreamHealthEvent::Failover, reason);
            active_route = next_route;
            continue;
        }
        match result {
            Ok((response, upstream_activity)) => {
                if matches!(failure, Some((UpstreamFailureKind::InvalidResponse, _))) {
                    drop(response);
                    return finish_proxy_unavailable(
                        &buffered_request,
                        "upstream_invalid_content_type",
                    )
                    .await;
                }
                if admission == UpstreamAttemptAdmission::Probe
                    && failure.is_none()
                    && response.status().is_success()
                {
                    match state
                        .db
                        .record_upstream_account_success(active_route.route.account_id)
                        .await
                    {
                        Ok(true) => state.metrics.observe_upstream_health(
                            UpstreamHealthEvent::Recovered,
                            UpstreamHealthReason::Success,
                        ),
                        Ok(false) => {}
                        Err(error) => tracing::warn!(
                            %request_id,
                            upstream_account_id = %active_route.route.account_id,
                            error = %error,
                            "failed to clear upstream account cooldown"
                        ),
                    }
                }
                break (response, upstream_activity);
            }
            Err(ProxySendError::Credential) => {
                return finish_proxy_failure(&buffered_request, "provider_credential").await;
            }
            Err(ProxySendError::RetryableConnection | ProxySendError::CandidateUnavailable) => {
                return finish_proxy_unavailable(&buffered_request, "upstream_connection").await;
            }
            Err(ProxySendError::NonRetryableTransport) => {
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
        return finish_buffered_request(
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
    }
    let content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();
    if is_codex_route && !codex_transport::is_event_stream(&upstream) {
        drop(upstream);
        return finish_proxy_failure(&buffered_request, "upstream_invalid_content_type").await;
    }
    if is_codex_route && !codex_downstream_stream {
        let buffered = match codex_transport::buffer_response(upstream).await {
            Ok(buffered) => buffered,
            Err(error_code) => {
                tracing::warn!(%request_id, stage = error_code, "Codex upstream response failed");
                return finish_proxy_failure(&buffered_request, error_code).await;
            }
        };
        return finish_buffered_request(
            &buffered_request,
            StatusCode::OK,
            buffered.body,
            "application/json",
            buffered.usage,
            None,
        )
        .await;
    }
    let is_sse = content_type
        .as_ref()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"));
    let capture_json_usage = should_capture_buffered_usage(is_sse, content_type.as_ref());
    if !is_sse {
        let response_content_type = content_type
            .as_ref()
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/json")
            .to_owned();
        let response_body = match read_bounded_upstream(upstream, MAX_PROXY_RESPONSE_BODY).await {
            Ok(body) => Bytes::from(body),
            Err(error) => {
                return finish_proxy_failure(&buffered_request, error.code()).await;
            }
        };
        if matches!(protocol, Protocol::OpenAiResponses)
            && let Err(error_code) = validate_buffered_responses_success(&response_body)
        {
            return finish_proxy_failure(&buffered_request, error_code).await;
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
                    return finish_proxy_failure(&buffered_request, "upstream_invalid_usage").await;
                }
            }
        } else {
            TokenUsage {
                input_tokens: input_token_ceiling,
                output_tokens: output_token_ceiling,
                ..TokenUsage::default()
            }
        };
        return finish_buffered_request(
            &buffered_request,
            status,
            response_body,
            &response_content_type,
            usage,
            None,
        )
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
        upstream_activity,
        request_id,
        buffered_request,
        proxy_lifecycle_permit,
    })
    .await
}

fn record_delivered_chunk(
    delivered_any: &mut bool,
    delivered_billable: &mut bool,
    chunk_billable: bool,
) {
    *delivered_any = true;
    *delivered_billable |= chunk_billable;
}

fn trusted_input_token_overhead_ceiling(
    route_driver: Option<&str>,
    route_config: Option<&Value>,
) -> Result<i64, AppError> {
    if route_driver != Some("http-json") {
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
            AppError::Upstream("HTTP JSON upstream input token overhead ceiling is invalid".into())
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
    let upstream_body = match read_bounded_upstream(upstream, maximum).await {
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
    response: reqwest::Response,
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

enum ExtractedUsage {
    Missing,
    Valid(TokenUsage),
    Invalid,
}

fn merge_streaming_usage(current: &mut TokenUsage, next: TokenUsage) -> Result<(), ()> {
    current.input_tokens = current.input_tokens.max(next.input_tokens);
    current.cached_input_tokens = current.cached_input_tokens.max(next.cached_input_tokens);
    current.cache_write_tokens = current.cache_write_tokens.max(next.cache_write_tokens);
    current.output_tokens = current.output_tokens.max(next.output_tokens);
    if let Some(next_tier) = next.service_tier {
        match current.service_tier.as_deref() {
            None => current.service_tier = Some(next_tier),
            Some(current_tier) if current_tier == next_tier => {}
            Some(_) => return Err(()),
        }
    }
    Ok(())
}

fn extract_usage_checked(body: &[u8]) -> ExtractedUsage {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        return match usage_from_value_checked(&value) {
            Ok(Some(usage)) => ExtractedUsage::Valid(usage),
            Ok(None) => ExtractedUsage::Missing,
            Err(()) => ExtractedUsage::Invalid,
        };
    }
    let mut result: Option<TokenUsage> = None;
    for line in body.split(|byte| *byte == b'\n') {
        let Some(line) = line
            .strip_prefix(b"data: ")
            .or_else(|| line.strip_prefix(b"data:"))
        else {
            continue;
        };
        let line = trim_ascii_whitespace(line);
        if line.is_empty() || line == b"[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            return ExtractedUsage::Invalid;
        };
        match usage_from_value_checked(&value) {
            Err(()) => return ExtractedUsage::Invalid,
            Ok(None) => continue,
            Ok(Some(next)) => {
                let current = result.get_or_insert_with(TokenUsage::default);
                if merge_streaming_usage(current, next).is_err() {
                    return ExtractedUsage::Invalid;
                }
            }
        }
    }
    match result {
        Some(usage) => ExtractedUsage::Valid(usage),
        None => ExtractedUsage::Missing,
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ResponsesSseEventKind {
    Lifecycle,
    Completed,
    Failed,
    Other,
}

impl ResponsesSseEventKind {
    fn from_name(name: &[u8]) -> Self {
        match trim_ascii_whitespace(name) {
            b"response.created" | b"response.in_progress" => Self::Lifecycle,
            b"response.completed" | b"message_stop" => Self::Completed,
            b"response.failed" | b"response.incomplete" | b"error" | b"response.error" => {
                Self::Failed
            }
            _ => Self::Other,
        }
    }

    fn is_response_lifecycle(self) -> bool {
        matches!(self, Self::Lifecycle | Self::Completed | Self::Failed)
    }
}

#[derive(Default)]
struct ResponsesSseCapture {
    line: Vec<u8>,
    data: Vec<u8>,
    event_kind: Option<ResponsesSseEventKind>,
    response_id: Option<String>,
    discard_line: bool,
    discard_event: bool,
    invalid: bool,
    terminal_success: bool,
    terminal_failure: bool,
    usage: Option<TokenUsage>,
    usage_invalid: bool,
    require_explicit_completed: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum ResponsesSseOutcome {
    Completed { response_id: Option<String> },
    Failed,
    Incomplete,
}

struct ResponsesSseSummary {
    outcome: ResponsesSseOutcome,
    usage: Option<TokenUsage>,
    usage_invalid: bool,
}

impl ResponsesSseCapture {
    fn for_responses() -> Self {
        Self {
            require_explicit_completed: true,
            ..Self::default()
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            if byte == b'\n' {
                self.finish_line();
            } else if self.discard_line {
                continue;
            } else if self.line.len() >= MAX_RESPONSES_SSE_EVENT_BYTES {
                self.line.clear();
                self.discard_line = true;
                self.discard_event = true;
                self.invalid = true;
            } else {
                self.line.push(byte);
            }
        }
    }

    fn finish_summary(mut self) -> ResponsesSseSummary {
        if !self.discard_line && !self.line.is_empty() {
            self.finish_line();
        }
        if !self.data.is_empty() || self.discard_event || self.event_kind.is_some() {
            self.dispatch_event();
        }
        let outcome = if self.terminal_failure {
            ResponsesSseOutcome::Failed
        } else if self.invalid {
            ResponsesSseOutcome::Incomplete
        } else if self.terminal_success {
            ResponsesSseOutcome::Completed {
                response_id: self.response_id,
            }
        } else {
            ResponsesSseOutcome::Incomplete
        };
        ResponsesSseSummary {
            outcome,
            usage: self.usage,
            usage_invalid: self.usage_invalid,
        }
    }

    #[cfg(test)]
    fn finish(self) -> ResponsesSseOutcome {
        self.finish_summary().outcome
    }

    fn finish_line(&mut self) {
        if self.discard_line {
            self.discard_line = false;
            self.line.clear();
            return;
        }
        let mut line = std::mem::take(&mut self.line);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.is_empty() {
            self.dispatch_event();
            return;
        }
        if self.discard_event {
            return;
        }
        if line == b"data" || line.starts_with(b"data:") {
            let value = if line == b"data" {
                &[][..]
            } else {
                line[5..].strip_prefix(b" ").unwrap_or(&line[5..])
            };
            let separator = usize::from(!self.data.is_empty());
            if self
                .data
                .len()
                .saturating_add(separator)
                .saturating_add(value.len())
                > MAX_RESPONSES_SSE_EVENT_BYTES
            {
                self.data.clear();
                self.discard_event = true;
                self.invalid = true;
                return;
            }
            if separator == 1 {
                self.data.push(b'\n');
            }
            self.data.extend_from_slice(value);
        } else if line == b"event" || line.starts_with(b"event:") {
            let value = if line == b"event" {
                &[][..]
            } else {
                line[6..].strip_prefix(b" ").unwrap_or(&line[6..])
            };
            self.event_kind = Some(ResponsesSseEventKind::from_name(value));
        }
    }

    fn dispatch_event(&mut self) {
        let data = std::mem::take(&mut self.data);
        let event_kind = self.event_kind.take();
        let discard = std::mem::take(&mut self.discard_event);
        if discard {
            match event_kind {
                Some(ResponsesSseEventKind::Completed) => self.terminal_success = true,
                Some(ResponsesSseEventKind::Failed) => self.terminal_failure = true,
                Some(ResponsesSseEventKind::Lifecycle | ResponsesSseEventKind::Other) | None => {}
            }
            return;
        }
        if data.is_empty() {
            match event_kind {
                Some(ResponsesSseEventKind::Completed) => self.terminal_success = true,
                Some(ResponsesSseEventKind::Failed) => self.terminal_failure = true,
                Some(ResponsesSseEventKind::Lifecycle | ResponsesSseEventKind::Other) | None => {}
            }
            return;
        }
        let data = trim_ascii_whitespace(&data);
        if data == b"[DONE]" {
            if matches!(event_kind, Some(ResponsesSseEventKind::Failed)) {
                if self.require_explicit_completed && self.terminal_failure {
                    self.invalid = true;
                }
                self.terminal_failure = true;
            } else if !self.require_explicit_completed {
                self.terminal_success = true;
            }
            return;
        }
        let Ok(value) = serde_json::from_slice::<Value>(data) else {
            self.invalid = true;
            return;
        };
        match usage_from_value_checked(&value) {
            Err(()) => self.usage_invalid = true,
            Ok(None) => {}
            Ok(Some(next)) => {
                let current = self.usage.get_or_insert_with(TokenUsage::default);
                if merge_streaming_usage(current, next).is_err() {
                    self.usage_invalid = true;
                }
            }
        }
        if value.get("error").is_some_and(|error| !error.is_null())
            || value
                .pointer("/response/error")
                .is_some_and(|error| !error.is_null())
        {
            self.terminal_failure = true;
        }
        let payload_kind = value
            .get("type")
            .and_then(Value::as_str)
            .map(|name| ResponsesSseEventKind::from_name(name.as_bytes()));
        if self.require_explicit_completed
            && let (Some(event_kind), Some(payload_kind)) = (event_kind, payload_kind)
            && event_kind != payload_kind
            && (event_kind.is_response_lifecycle() || payload_kind.is_response_lifecycle())
        {
            self.invalid = true;
            if matches!(event_kind, ResponsesSseEventKind::Failed)
                || matches!(payload_kind, ResponsesSseEventKind::Failed)
            {
                self.terminal_failure = true;
            }
            return;
        }
        let kind = payload_kind
            .or(event_kind)
            .unwrap_or(ResponsesSseEventKind::Other);
        if kind.is_response_lifecycle()
            && let Some(response_id) = value
                .pointer("/response/id")
                .or_else(|| value.get("id"))
                .and_then(Value::as_str)
                .and_then(safe_conversation_hint)
        {
            match self.response_id.as_deref() {
                None => self.response_id = Some(response_id),
                Some(current) if current == response_id => {}
                Some(_) => self.invalid = true,
            }
        }
        match kind {
            ResponsesSseEventKind::Completed => {
                if self.require_explicit_completed
                    && (self.terminal_success || self.terminal_failure)
                {
                    self.invalid = true;
                }
                self.terminal_success = true;
            }
            ResponsesSseEventKind::Failed => {
                if self.require_explicit_completed
                    && (self.terminal_success || self.terminal_failure)
                {
                    self.invalid = true;
                }
                self.terminal_failure = true;
            }
            ResponsesSseEventKind::Lifecycle | ResponsesSseEventKind::Other => {}
        }
    }
}

fn trim_ascii_whitespace(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

fn should_capture_buffered_usage(is_sse: bool, content_type: Option<&HeaderValue>) -> bool {
    if is_sse {
        return false;
    }
    let Some(media_type) = content_type
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
    else {
        // Some compatible upstreams omit Content-Type even though their body
        // is JSON. Preserve the existing usage parsing contract for them.
        return content_type.is_none();
    };
    media_type.eq_ignore_ascii_case("application/json")
        || media_type
            .get(media_type.len().saturating_sub("+json".len())..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case("+json"))
}

#[cfg(test)]
fn completed_response_id(
    status: StatusCode,
    transport_complete: bool,
    responses_sse: bool,
    streamed_response_id: Option<String>,
    buffered_tail: &[u8],
) -> Option<String> {
    if !status.is_success() || !transport_complete {
        return None;
    }
    if responses_sse {
        streamed_response_id
    } else {
        extract_response_id(buffered_tail)
    }
}

fn extract_response_id(body: &[u8]) -> Option<String> {
    const MAX_RESPONSE_ID_SCAN_BYTES: usize = 2 * 1024 * 1024;

    fn id_from_value(value: &Value) -> Option<String> {
        value
            .pointer("/response/id")
            .or_else(|| value.get("id"))
            .and_then(Value::as_str)
            .and_then(safe_conversation_hint)
    }

    let body = body.get(..body.len().min(MAX_RESPONSE_ID_SCAN_BYTES))?;
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        return id_from_value(&value);
    }

    let mut top_level_id = None;
    for line in body.split(|byte| *byte == b'\n') {
        let Some(data) = line.strip_prefix(b"data:") else {
            continue;
        };
        let data = data.strip_prefix(b" ").unwrap_or(data);
        let data = data.strip_suffix(b"\r").unwrap_or(data);
        if data == b"[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(data) else {
            continue;
        };
        if let Some(response_id) = value
            .pointer("/response/id")
            .and_then(Value::as_str)
            .and_then(safe_conversation_hint)
        {
            return Some(response_id);
        }
        if top_level_id.is_none() {
            top_level_id = value
                .get("id")
                .and_then(Value::as_str)
                .and_then(safe_conversation_hint);
        }
    }
    top_level_id
}

fn usage_from_value_checked(value: &Value) -> Result<Option<TokenUsage>, ()> {
    let Some(usage) = value
        .get("usage")
        .or_else(|| value.pointer("/message/usage"))
        .or_else(|| value.pointer("/response/usage"))
    else {
        return Ok(None);
    };
    if usage.is_null() {
        return Ok(None);
    }
    let usage = usage.as_object().ok_or(())?;
    let integer = |field: &str| -> Result<Option<i64>, ()> {
        usage
            .get(field)
            .map(|value| value.as_i64().ok_or(()))
            .transpose()
    };
    let input = match integer("input_tokens")? {
        Some(value) => Some(value),
        None => integer("prompt_tokens")?,
    };
    let output = match integer("output_tokens")? {
        Some(value) => Some(value),
        None => integer("completion_tokens")?,
    };
    let (reported_input, output) = match (input, output) {
        (Some(input), Some(output)) => (input, output),
        (Some(input), None) => (input, 0),
        (None, Some(output)) => (0, output),
        // Some OpenAI-compatible providers emit a metadata-only `usage`
        // object (for example only `total_tokens`). Treat that exactly like
        // omitted usage so the caller charges the already-reserved ceilings.
        // A present input/output field with an invalid type still fails above.
        (None, None) => return Ok(None),
    };
    let details_integer = |details_field: &str| -> Result<Option<i64>, ()> {
        let Some(details) = usage.get(details_field) else {
            return Ok(None);
        };
        let details = details.as_object().ok_or(())?;
        details
            .get("cached_tokens")
            .map(|value| value.as_i64().ok_or(()))
            .transpose()
    };
    let cached_input = match details_integer("input_tokens_details")? {
        Some(value) => value,
        None => match details_integer("prompt_tokens_details")? {
            Some(value) => value,
            None => integer("cache_read_input_tokens")?.unwrap_or_default(),
        },
    };
    let cache_write = if let Some(value) = integer("cache_creation_input_tokens")? {
        value
    } else if let Some(details) = usage.get("cache_creation") {
        let details = details.as_object().ok_or(())?;
        let detail_integer = |field: &str| -> Result<i64, ()> {
            details
                .get(field)
                .map(|value| value.as_i64().ok_or(()))
                .transpose()
                .map(Option::unwrap_or_default)
        };
        detail_integer("ephemeral_5m_input_tokens")?
            .checked_add(detail_integer("ephemeral_1h_input_tokens")?)
            .ok_or(())?
    } else {
        0
    };
    // OpenAI prompt/input counts include cached tokens; Anthropic input_tokens
    // excludes its separately reported cache read/write counters.
    let input_includes_cache = usage.contains_key("input_tokens_details")
        || usage.contains_key("prompt_tokens_details")
        || usage.contains_key("prompt_tokens");
    let uncached_input = if input_includes_cache {
        reported_input.checked_sub(cached_input).ok_or(())?
    } else {
        reported_input
    };
    let service_tier_value = value
        .get("service_tier")
        .or_else(|| value.pointer("/response/service_tier"));
    let service_tier = match service_tier_value {
        None => None,
        Some(value) => {
            let tier = value.as_str().ok_or(())?;
            if !is_supported_service_tier(tier) {
                return Err(());
            }
            Some(tier.to_owned())
        }
    };
    let parsed = TokenUsage {
        input_tokens: uncached_input,
        cached_input_tokens: cached_input,
        cache_write_tokens: cache_write,
        output_tokens: output,
        service_tier,
    };
    if [
        parsed.input_tokens,
        parsed.cached_input_tokens,
        parsed.cache_write_tokens,
        parsed.output_tokens,
    ]
    .into_iter()
    .all(|tokens| (0..=MAX_REPORTED_TOKENS).contains(&tokens))
        && parsed
            .input_tokens
            .checked_add(parsed.cached_input_tokens)
            .and_then(|tokens| tokens.checked_add(parsed.cache_write_tokens))
            .is_some()
    {
        Ok(Some(parsed))
    } else {
        Err(())
    }
}

#[cfg(test)]
fn usage_from_value(value: &Value) -> Option<TokenUsage> {
    usage_from_value_checked(value).ok().flatten()
}

fn is_supported_service_tier(tier: &str) -> bool {
    matches!(
        tier,
        "default" | "auto" | "priority" | "flex" | "scale" | "batch" | "standard_only"
    )
}

fn append_bounded(capture: &mut Vec<u8>, chunk: &[u8], maximum: usize) {
    if chunk.len() >= maximum {
        capture.clear();
        capture.extend_from_slice(&chunk[chunk.len() - maximum..]);
        return;
    }
    let overflow = capture
        .len()
        .saturating_add(chunk.len())
        .saturating_sub(maximum);
    if overflow > 0 {
        capture.drain(..overflow);
    }
    capture.extend_from_slice(chunk);
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
