use super::*;
use crate::db::UpstreamFailureKind;

mod admission;
mod candidates;
mod clock;
mod codex;
pub(super) mod diagnostics;
mod http;
pub(super) mod kimi;
mod outcome;
mod policy;
mod probe;
mod readiness;
pub(super) mod recovery_wait;
mod route_types;

pub(super) use crate::provider::PROXY_ROUTING_POLICY;
pub(super) use admission::{
    AdmittedProxyRouteInput, DeferredSharedProbe, NextSendableProxyRouteInput,
    prepare_admitted_proxy_route,
};
pub(super) use candidates::{
    CandidatePreparationSummary, candidate_reservation_bounds, exhausted_candidate_error,
    next_planned_proxy_candidate, prepared_input_reservation_bound, retain_pinned_text_candidates,
};
pub(super) use clock::credential_application_now;
#[cfg(test)]
pub(super) use clock::with_test_credential_application_now_once;
pub(super) use codex::quota::classify_rate_limit;
#[cfg(test)]
pub(super) use codex::with_test_pre_delivery_connect_failures;
pub(super) use codex::{CodexRetryTerminal, CodexRetryTerminalGuard, runtime_transport_policy};
pub(super) use outcome::{classify_attempt_failure, failover_disposition};
pub(crate) use policy::RequestAttemptBudget;
pub(super) use probe::{SharedProbePermit, join_shared_probe};
pub(crate) use probe::{UpstreamAttemptGuard, UpstreamAttemptTerminal};
pub(super) use readiness::{
    CandidateCompatibility, PreparedRouteReadiness, candidate_compatibility,
    credential_application_error, refresh_route_snapshot,
};
pub(crate) use recovery_wait::wait as wait_media_recovery;
pub(super) use route_types::{
    PlannedProxyRoute, PreparedProxyRoute, ProxyRequestContext, ProxyRoutePlanInput,
    ProxyRouteResponse, ProxySendError, TransportFailureKind,
};

pub(super) fn plan_proxy_route(
    input: ProxyRoutePlanInput<'_>,
) -> Result<PlannedProxyRoute, AppError> {
    let ProxyRoutePlanInput {
        request:
            ProxyRequestContext {
                state,
                key,
                model,
                protocol,
                request_id,
                request_json,
                codex_multi_agent_v2_client,
                codex_multi_agent_v2_tools_prepared,
            },
        route,
        preparation_now,
    } = input;
    if !state.providers.is_public(&route.driver) {
        return Err(AppError::Upstream(format!(
            "provider driver {} is not loaded",
            route.driver
        )));
    }
    route.credential.validate(preparation_now)?;
    let _planning_memory = state
        .proxy_memory_budget
        .temporary(
            crate::gateway_body::memory::json_encoded_length(request_json)?.saturating_mul(3),
        )
        .map_err(|error| {
            state
                .metrics
                .observe_proxy_memory_error(crate::metrics::ProxyMemoryRejectionStage::Route, error)
        })?;
    let is_codex = codex_transport::is_driver(&route.driver);
    if is_codex {
        codex::validate_route(&route, protocol)?;
    }
    let (mut forwarded_json, responses_chat) = kimi::prepare_forwarded_request(
        &route,
        protocol,
        request_json,
        matches!(protocol, Protocol::OpenAiResponses)
            && codex_multi_agent_v2_client
            && codex_multi_agent_v2_tools_prepared,
        matches!(protocol, Protocol::OpenAiResponses)
            && codex_multi_agent_v2_client
            && state.providers.supports_codex_multi_agent_v2(&route.driver),
        matches!(protocol, Protocol::OpenAiResponses)
            && state
                .providers
                .supports_responses_via_chat_v1(&route.driver),
    )?;
    let codex_plan = if is_codex {
        Some(codex_transport::prepare_request_with_id(
            &mut forwarded_json,
            &route.upstream_model,
            &route.config,
            request_id,
            protocol,
        )?)
    } else {
        None
    };
    let output_token_ceiling = match codex_plan.as_ref() {
        Some(plan) => plan.output_token_ceiling,
        None => inject_controlled_output_ceiling(
            if responses_chat.is_some() {
                Protocol::OpenAiChat
            } else {
                protocol
            },
            &mut forwarded_json,
        )?,
    };
    let upstream_stream = forwarded_json.get("stream").and_then(Value::as_bool) == Some(true);
    let component_adapter = state
        .providers
        .get(&route.driver)
        .and_then(|provider| provider.component_adapter.as_ref());
    let component_context = if component_adapter.is_some() {
        match forwarded_json.get("stream") {
            Some(Value::Bool(false)) | None => {}
            Some(Value::Bool(true)) => {
                return Err(AppError::BadRequest(
                    "component providers support buffered requests only; stream=true is unavailable"
                        .into(),
                ));
            }
            Some(_) => return Err(AppError::BadRequest("stream must be a boolean".into())),
        }
        let context = RequestContext {
            tenant_id: key.tenant_id.to_string(),
            principal_id: key.principal_id.to_string(),
            key_id: key.key_id.to_string(),
            protocol: protocol.name().to_owned(),
            model: model.to_owned(),
            config_json: serde_json::to_string(&route.config).map_err(|_| AppError::Internal)?,
        };
        Some(context)
    } else {
        None
    };
    let codex_downstream_stream = codex_plan
        .as_ref()
        .is_some_and(|plan| plan.downstream_stream);
    let codex_store_disabled =
        codex_plan.is_some() && forwarded_json.get("store").and_then(Value::as_bool) == Some(false);
    let codex_session_id = codex_plan.map(|plan| plan.session_id);
    Ok(PlannedProxyRoute {
        route,
        forwarded_json,
        output_token_ceiling,
        upstream_stream,
        codex_downstream_stream,
        codex_store_disabled,
        codex_session_id,
        component_context,
        responses_chat,
    })
}

pub(super) async fn materialize_proxy_route(
    state: &AppState,
    planned: PlannedProxyRoute,
) -> Result<PreparedProxyRoute, AppError> {
    let encoded_length = crate::gateway_body::memory::json_encoded_length(&planned.forwarded_json)?;
    // Allocate temporary serialization/adapter work before cloning any payload.
    let temporary_bytes = if planned.component_context.is_some() {
        64 * 1024 * 1024 + encoded_length.saturating_mul(6)
    } else {
        encoded_length.saturating_mul(2)
    };
    let _temporary_memory = state
        .proxy_memory_budget
        .temporary(temporary_bytes)
        .map_err(|error| {
            state
                .metrics
                .observe_proxy_memory_error(crate::metrics::ProxyMemoryRejectionStage::Route, error)
        })?;
    let component_request = if let Some(context) = planned.component_context {
        let prepared = prepare_component_provider(
            state,
            &planned.route.driver,
            context.clone(),
            planned.route.config.clone(),
            planned.forwarded_json.clone(),
            _temporary_memory.clone(),
        )
        .await?;
        Some((prepared, context))
    } else {
        None
    };
    let mut forwarded_body = Vec::with_capacity(encoded_length);
    serde_json::to_writer(&mut forwarded_body, &planned.forwarded_json)
        .map_err(|_| AppError::Internal)?;
    Ok(PreparedProxyRoute {
        route: planned.route,
        forwarded_body: Bytes::from(forwarded_body),
        upstream_stream: planned.upstream_stream,
        codex_downstream_stream: planned.codex_downstream_stream,
        codex_store_disabled: planned.codex_store_disabled,
        codex_session_id: planned.codex_session_id,
        component_request,
        responses_chat: planned.responses_chat,
    })
}

pub(super) async fn send_proxy_route(
    state: &AppState,
    headers: &HeaderMap,
    protocol: Protocol,
    request_id: Uuid,
    route: &PreparedProxyRoute,
    candidate_rank: usize,
    outbound_attempt: usize,
) -> Result<ProxyRouteResponse, ProxySendError> {
    // Catalog/quota support does not implement Cursor's AgentService Run
    // conversation protocol. Never send its OAuth token and an OpenAI payload
    // through the generic HTTP fallback. This is a local, undispatched skip,
    // not supplier failure or evidence that a retry would consume tokens.
    if route.route.driver == crate::cursor_native::DRIVER {
        return Err(ProxySendError::CandidateUnavailable);
    }
    if route.is_codex() {
        return codex::send_proxy_route(
            state,
            headers,
            request_id,
            route,
            candidate_rank,
            outbound_attempt,
        )
        .await;
    }
    http::send_reqwest_proxy_route(state, headers, protocol, request_id, route).await
}

pub(super) fn retryable_upstream_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
    ) || status.is_server_error()
}
