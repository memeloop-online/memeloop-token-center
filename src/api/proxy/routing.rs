use super::*;
use crate::db::UpstreamFailureKind;

mod admission;
mod clock;
mod codex;
mod http;
mod kimi;
mod outcome;
mod probe;
mod readiness;

pub(super) use crate::provider::PROXY_ROUTING_POLICY;
pub(super) use admission::{
    AdmittedProxyRouteInput, DeferredSharedProbe, NextSendableProxyRouteInput,
    prepare_admitted_proxy_route,
};
pub(super) use clock::credential_application_now;
#[cfg(test)]
pub(super) use clock::with_test_credential_application_now_once;
pub(super) use codex::quota::classify_rate_limit;
#[cfg(test)]
pub(super) use codex::with_test_pre_delivery_connect_failures;
pub(super) use codex::{
    CodexRetryTerminal, CodexRetryTerminalGuard, runtime_transport_policy,
};
pub(super) use outcome::classify_attempt_failure;
pub(super) use probe::{
    SharedProbePermit, UpstreamAttemptGuard, UpstreamAttemptTerminal, join_shared_probe,
};
pub(super) use readiness::{
    CandidateCompatibility, PreparedRouteReadiness, candidate_compatibility,
    credential_application_error, refresh_route_snapshot,
};

pub(super) struct PreparedProxyRoute {
    pub(super) route: ResolvedUpstream,
    forwarded_body: Vec<u8>,
    pub(super) upstream_stream: bool,
    pub(super) codex_downstream_stream: bool,
    pub(super) codex_store_disabled: bool,
    codex_session_id: Option<String>,
    pub(super) component_request: Option<(PreparedProviderRequest, RequestContext)>,
    kimi_response: Option<crate::api::kimi_transport::responses::Context>,
}

pub(super) struct PlannedProxyRoute {
    pub(super) route: ResolvedUpstream,
    forwarded_json: Value,
    pub(super) output_token_ceiling: i64,
    upstream_stream: bool,
    codex_downstream_stream: bool,
    codex_store_disabled: bool,
    codex_session_id: Option<String>,
    component_context: Option<RequestContext>,
    kimi_response: Option<crate::api::kimi_transport::responses::Context>,
}

impl PlannedProxyRoute {
    pub(super) fn is_component(&self) -> bool {
        self.component_context.is_some()
    }

    pub(super) fn request_body_ceiling(
        &self,
        original_body_length: usize,
    ) -> Result<usize, AppError> {
        let forwarded_length = serde_json::to_vec(&self.forwarded_json)
            .map_err(|_| AppError::Internal)?
            .len();
        Ok(original_body_length.max(forwarded_length))
    }
}

impl PreparedProxyRoute {
    pub(super) fn is_codex(&self) -> bool {
        codex_transport::is_driver(&self.route.driver)
    }

    pub(super) fn request_body_ceiling(&self, original_body_length: usize) -> usize {
        original_body_length.max(self.forwarded_body.len()).max(
            self.component_request
                .as_ref()
                .map(|(request, _)| request.body.len())
                .unwrap_or_default(),
        )
    }
}

#[derive(Clone, Copy)]
pub(super) struct ProxyRequestContext<'a> {
    pub(super) state: &'a AppState,
    pub(super) key: &'a AuthenticatedKey,
    pub(super) model: &'a str,
    pub(super) protocol: Protocol,
    pub(super) request_id: Uuid,
    pub(super) request_json: &'a Value,
}

pub(super) struct ProxyRoutePlanInput<'a> {
    pub(super) request: ProxyRequestContext<'a>,
    pub(super) route: ResolvedUpstream,
    pub(super) preparation_now: i64,
}

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
            },
        route,
        preparation_now,
    } = input;
    if !state.providers.contains(&route.driver) {
        return Err(AppError::Upstream(format!(
            "provider driver {} is not loaded",
            route.driver
        )));
    }
    route.credential.validate(preparation_now)?;
    let is_codex = codex_transport::is_driver(&route.driver);
    if is_codex {
        codex::validate_route(&route, protocol)?;
    }
    let (mut forwarded_json, kimi_response) =
        kimi::prepare_forwarded_request(&route, protocol, request_json)?;
    let codex_plan = if is_codex {
        Some(codex_transport::prepare_request_with_id(
            &mut forwarded_json,
            &route.upstream_model,
            &route.config,
            request_id,
        )?)
    } else {
        None
    };
    let output_token_ceiling = match codex_plan.as_ref() {
        Some(plan) => plan.output_token_ceiling,
        None => inject_controlled_output_ceiling(
            if kimi_response.is_some() {
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
        kimi_response,
    })
}

pub(super) async fn materialize_proxy_route(
    state: &AppState,
    planned: PlannedProxyRoute,
) -> Result<PreparedProxyRoute, AppError> {
    let component_request = if let Some(context) = planned.component_context {
        let prepared = prepare_component_provider(
            state,
            &planned.route.driver,
            context.clone(),
            planned.route.config.clone(),
            planned.forwarded_json.clone(),
        )
        .await?;
        Some((prepared, context))
    } else {
        None
    };
    let forwarded_body =
        serde_json::to_vec(&planned.forwarded_json).map_err(|_| AppError::Internal)?;
    Ok(PreparedProxyRoute {
        route: planned.route,
        forwarded_body,
        upstream_stream: planned.upstream_stream,
        codex_downstream_stream: planned.codex_downstream_stream,
        codex_store_disabled: planned.codex_store_disabled,
        codex_session_id: planned.codex_session_id,
        component_request,
        kimi_response: planned.kimi_response,
    })
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum ProxySendError {
    RetryableConnection(&'static str),
    RetryableCodexBadRequest,
    CodexBadRequest,
    CandidateUnavailable,
    AmbiguousResponse(&'static str),
    NonRetryableTransport,
    CredentialUnavailable,
    Credential,
}

pub(super) struct ProxyRouteResponse {
    pub(super) response: UpstreamResponse,
    pub(super) upstream_activity: crate::metrics::ActivityGuard,
    pub(super) codex_retry: CodexRetryTerminalGuard,
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
