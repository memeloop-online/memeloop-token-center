use super::*;
use crate::db::UpstreamFailureKind;

mod admission;
mod anthropic;
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
pub(super) use outcome::{attempt_failure_stage, classify_attempt_failure, failover_disposition};
pub(crate) use policy::RequestAttemptBudget;
pub(super) use probe::{SharedProbePermit, join_shared_probe};
pub(crate) use probe::{
    UpstreamAttemptGuard, UpstreamAttemptTerminal, current_gateway_failure_domain,
};
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
                codex_multi_agent_v2_request,
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
    let responses_via_chat_dialect = state
        .providers
        .responses_via_chat_dialect(&route.driver, &route.config);
    let http_json = crate::provider::is_openai_compatible_http_driver(&route.driver);
    let new_api = crate::provider::is_new_api_driver(&route.driver);
    let passthrough_responses = http_json || new_api;
    let multi_agent_responses =
        matches!(protocol, Protocol::OpenAiResponses) && codex_multi_agent_v2_request;
    // Kimi still rewrites MultiAgent into Chat. http-json and New API keep
    // native Responses semantics; do not reuse the Kimi rewrite for them.
    let bridge_multi_agent = matches!(protocol, Protocol::OpenAiResponses)
        && codex_multi_agent_v2_request
        && state
            .providers
            .supports_codex_multi_agent_v2(&route.driver, &route.config);
    let responses_via_anthropic = matches!(protocol, Protocol::OpenAiResponses)
        && state
            .providers
            .supports_responses_via_anthropic_messages_v1(&route.driver);
    let (mut forwarded_json, responses_chat, responses_anthropic) = if responses_via_anthropic {
        let (forwarded, context) =
            anthropic::prepare_forwarded_request(&route, request_json, bridge_multi_agent)?;
        (forwarded, None, Some(context))
    } else {
        let (forwarded, context) = kimi::prepare_forwarded_request(
            &route,
            protocol,
            request_json,
            bridge_multi_agent && responses_via_chat_dialect.is_some(),
            bridge_multi_agent && responses_via_chat_dialect.is_some(),
            responses_via_chat_dialect,
        )?;
        (forwarded, context, None)
    };
    let (compact_v2_bridge, wrap_compact_as_sse) = prepare_compact_bridge(
        &route,
        protocol,
        request_json,
        passthrough_responses,
        &mut forwarded_json,
    )?;
    let upstream_path = if responses_anthropic.is_some() {
        Protocol::AnthropicMessages.path()
    } else {
        upstream_path_for(protocol, compact_v2_bridge, responses_chat.is_some())
    };
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
        None if compact_v2_bridge
            || matches!(
                protocol,
                Protocol::OpenAiResponsesCompact | Protocol::OpenAiAlphaSearch
            )
            || (http_json && multi_agent_responses)
            || (new_api
                && matches!(
                    protocol,
                    Protocol::OpenAiResponses
                        | Protocol::OpenAiResponsesCompact
                        | Protocol::OpenAiAlphaSearch
                )) =>
        {
            passthrough_output_reservation_bound(
                &forwarded_json,
                &route.config,
                &route.upstream_model,
            )?
        }
        None => inject_controlled_output_ceiling(
            if responses_chat.is_some() {
                Protocol::OpenAiChat
            } else if responses_anthropic.is_some() {
                Protocol::AnthropicMessages
            } else {
                protocol
            },
            &mut forwarded_json,
        )?,
    };
    let upstream_stream =
        forwarded_json.get("stream").and_then(Value::as_bool) == Some(true) && !compact_v2_bridge;
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
    let plugins = wire_shim_runtime(state);
    let wire_protocol = if responses_anthropic.is_some() {
        "anthropic"
    } else {
        protocol.name()
    };
    let wire_shim_context = (upstream_path == Protocol::AnthropicMessages.path()
        && plugins.wire_shim_matches(&route.driver, wire_protocol))
    .then(
        || crate::plugin::memeloop::token_center::types::RequestContext {
            tenant_id: key.tenant_id.to_string(),
            principal_id: key.principal_id.to_string(),
            key_id: key.key_id.to_string(),
            protocol: wire_protocol.to_owned(),
            model: model.to_owned(),
            config_json: "{}".to_owned(),
        },
    );
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
        upstream_path,
        compact_v2_bridge,
        wrap_compact_as_sse,
        responses_anthropic,
        wire_shim_context,
    })
}

fn wire_shim_runtime(state: &AppState) -> crate::plugin::PluginRuntime {
    #[cfg(feature = "experimental-plugin-revisions")]
    if let Some(snapshot) = &state.pinned_application_plugins {
        return snapshot.runtime.runtime().clone();
    }
    state.plugins.clone()
}

pub(super) fn passthrough_output_reservation_bound(
    request: &Value,
    config: &Value,
    upstream_model: &str,
) -> Result<i64, AppError> {
    if let Some(limit) = request.get("max_output_tokens") {
        return limit
            .as_i64()
            .filter(|limit| (0..=MAX_REPORTED_TOKENS).contains(limit))
            .ok_or_else(|| AppError::BadRequest("max_output_tokens is invalid".into()));
    }
    // Pass-through envelopes cannot assume an output cap that was never sent
    // to the supplier. Reserve a trusted per-model bound before dispatch;
    // actual usage still must fit the reservation at settlement.
    if let Some(bounds) = config.get("reservation_token_bounds") {
        let bounds = bounds.as_object().ok_or_else(|| {
            AppError::Upstream("compatible upstream reservation metadata must be an object".into())
        })?;
        // Generic accounts can opt in one model at a time. An absent entry
        // preserves that model's legacy admission; native Codex remains strict.
        if !bounds.contains_key(upstream_model) {
            return Ok(4_096);
        }
        return codex_transport::trusted_reservation_token_bound(config, upstream_model).map_err(
            |_| {
                AppError::Upstream(
                    "compatible upstream requires valid reservation metadata for its model".into(),
                )
            },
        );
    }
    // Preserve existing admission for accounts not yet configured with
    // trusted metadata. Their historical bound is not a supplier output cap.
    Ok(4_096)
}

fn prepare_compact_bridge(
    route: &ResolvedUpstream,
    protocol: Protocol,
    request_json: &Value,
    passthrough_responses: bool,
    forwarded_json: &mut Value,
) -> Result<(bool, bool), AppError> {
    let compact_v2_bridge = matches!(protocol, Protocol::OpenAiResponses)
        && crate::api::new_api_transport::has_compaction_trigger(request_json)
        && (crate::provider::is_new_api_driver(&route.driver)
            || (crate::provider::is_openai_compatible_http_driver(&route.driver)
                && route
                    .config
                    .get("responses_compact_v2_bridge")
                    .and_then(Value::as_bool)
                    == Some(true)));
    let wrap_compact_as_sse =
        compact_v2_bridge && request_json.get("stream").and_then(Value::as_bool) == Some(true);
    if passthrough_responses
        && (compact_v2_bridge || matches!(protocol, Protocol::OpenAiResponsesCompact))
    {
        *forwarded_json = crate::api::new_api_transport::prepare_compact_request(
            forwarded_json,
            &route.upstream_model,
        )?;
    }
    Ok((compact_v2_bridge, wrap_compact_as_sse))
}

fn upstream_path_for(
    protocol: Protocol,
    compact_v2_bridge: bool,
    responses_chat: bool,
) -> &'static str {
    if compact_v2_bridge || matches!(protocol, Protocol::OpenAiResponsesCompact) {
        crate::api::new_api_transport::compact_path()
    } else if matches!(protocol, Protocol::OpenAiAlphaSearch) {
        crate::api::new_api_transport::alpha_search_path()
    } else if responses_chat {
        Protocol::OpenAiChat.path()
    } else {
        protocol.path()
    }
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
    if let Some(context) = planned.wire_shim_context {
        let plugins = wire_shim_runtime(state);
        let configurations = plugins
            .resolved_traffic_configurations(
                context.tenant_id.parse().map_err(|_| AppError::Internal)?,
            )
            .await?;
        let body = String::from_utf8(forwarded_body).map_err(|_| AppError::Internal)?;
        let driver = planned.route.driver.clone();
        let protocol = context.protocol.clone();
        let outcome = crate::api::plugin_execution::run(
            state.metrics.clone(),
            crate::api::plugin_execution::Phase::WireShimFinalize,
            move || {
                plugins.finalize_wire_shim(
                    &driver,
                    &protocol,
                    context,
                    &body,
                    "{}",
                    &configurations,
                )
            },
        )
        .await?
        .ok_or(AppError::Internal)?;
        forwarded_body = outcome.request_json.into_bytes();
    }
    Ok(PreparedProxyRoute {
        route: planned.route,
        forwarded_body: Bytes::from(forwarded_body),
        upstream_stream: planned.upstream_stream,
        codex_downstream_stream: planned.codex_downstream_stream,
        codex_store_disabled: planned.codex_store_disabled,
        codex_session_id: planned.codex_session_id,
        component_request,
        responses_chat: planned.responses_chat,
        upstream_path: planned.upstream_path,
        compact_v2_bridge: planned.compact_v2_bridge,
        wrap_compact_as_sse: planned.wrap_compact_as_sse,
        responses_anthropic: planned.responses_anthropic,
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

#[cfg(test)]
mod compact_bridge_tests {
    use super::*;
    use serde_json::json;

    fn route(driver: &str, config: Value) -> ResolvedUpstream {
        ResolvedUpstream {
            route_id: Uuid::nil(),
            account_id: Uuid::nil(),
            transport_revision: 1,
            credential_generation: 1,
            driver: driver.into(),
            base_url: "https://upstream.invalid".into(),
            config,
            upstream_model: "upstream-model".into(),
            credential: crate::provider::UpstreamCredential::None,
        }
    }

    #[test]
    fn http_json_compaction_bridge_requires_account_opt_in() {
        let request = json!({
            "model": "public-model",
            "stream": true,
            "tools": [{"type": "function", "name": "exec"}],
            "input": [
                {"type": "message", "role": "user", "content": "earlier"},
                {"type": "compaction_trigger"}
            ]
        });
        for (driver, config, expected_bridge) in [
            ("http-json", json!({}), false),
            (
                "http-json",
                json!({"responses_compact_v2_bridge": false}),
                false,
            ),
            ("cbcnx", json!({}), false),
            (
                "http-json",
                json!({"responses_compact_v2_bridge": true}),
                true,
            ),
            ("new-api", json!({}), true),
        ] {
            let route = route(driver, config);
            let (mut forwarded, context) = kimi::prepare_forwarded_request(
                &route,
                Protocol::OpenAiResponses,
                &request,
                false,
                false,
                None,
            )
            .unwrap();
            assert!(context.is_none());
            let (bridge, wrap_sse) = prepare_compact_bridge(
                &route,
                Protocol::OpenAiResponses,
                &request,
                true,
                &mut forwarded,
            )
            .unwrap();
            assert_eq!(bridge, expected_bridge, "{driver}");
            assert_eq!(wrap_sse, expected_bridge, "{driver}");
            assert_eq!(
                upstream_path_for(Protocol::OpenAiResponses, bridge, false),
                if expected_bridge {
                    "/v1/responses/compact"
                } else {
                    "/v1/responses"
                },
                "{driver}"
            );
            if expected_bridge {
                assert_eq!(forwarded["stream"], false);
                assert_eq!(forwarded["input"].as_array().unwrap().len(), 1);
                assert!(forwarded.get("tools").is_none());
            } else {
                let mut original = request.clone();
                original["model"] = json!("upstream-model");
                assert_eq!(forwarded, original, "{driver}: unchanged request body");
            }
        }
    }

    #[test]
    fn explicit_compact_protocol_keeps_its_existing_path() {
        let route = route("http-json", json!({}));
        let mut forwarded = json!({"model": "upstream-model", "input": "history"});
        let (bridge, wrap_sse) = prepare_compact_bridge(
            &route,
            Protocol::OpenAiResponsesCompact,
            &forwarded.clone(),
            true,
            &mut forwarded,
        )
        .unwrap();
        assert!(!bridge);
        assert!(!wrap_sse);
        assert_eq!(
            upstream_path_for(Protocol::OpenAiResponsesCompact, bridge, false),
            "/v1/responses/compact"
        );
        assert_eq!(forwarded["input"][0]["content"], "history");
    }
}
