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
    ProxyRouteResponse, ProxySendError, TransportFailureKind, WireShimPlan,
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
                headers,
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
    let responses_via_chat_dialect = state.providers.responses_via_chat_dialect(&route.driver);
    let (mut forwarded_json, responses_chat) = kimi::prepare_forwarded_request(
        &route,
        protocol,
        request_json,
        matches!(protocol, Protocol::OpenAiResponses) && codex_multi_agent_v2_request,
        matches!(protocol, Protocol::OpenAiResponses)
            && codex_multi_agent_v2_request
            && state.providers.supports_codex_multi_agent_v2(&route.driver),
        responses_via_chat_dialect,
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
    let wire_shim = plan_wire_shim(
        state,
        headers,
        &route.driver,
        protocol,
        key,
        model,
        upstream_stream,
        responses_chat.is_some(),
        is_codex,
        component_context.is_some(),
    )?;
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
        wire_shim,
    })
}

/// The wire-shim hook applies to the generic reqwest HTTP path only: Codex
/// transport and component providers replace the serialized body afterwards,
/// so a byte-exact finalize would be silently dropped on those paths.
/// Effective wire-shim runtime for this request. When the
/// experimental-plugin-revisions pin taken at proxy entry carries an
/// application-plugin snapshot, the hook set and plugin configuration resolve
/// against that snapshot; otherwise the load-time baseline is used.
fn wire_shim_runtime(state: &AppState) -> crate::plugin::PluginRuntime {
    #[cfg(feature = "experimental-plugin-revisions")]
    if let Some(snapshot) = &state.pinned_application_plugins {
        return snapshot.runtime.runtime().clone();
    }
    state.plugins.clone()
}

#[allow(clippy::too_many_arguments)]
fn plan_wire_shim(
    state: &AppState,
    headers: &HeaderMap,
    driver: &str,
    protocol: Protocol,
    key: &AuthenticatedKey,
    model: &str,
    upstream_stream: bool,
    responses_chat: bool,
    is_codex: bool,
    is_component: bool,
) -> Result<Option<WireShimPlan>, AppError> {
    if is_codex
        || is_component
        || !wire_shim_runtime(state).wire_shim_matches(driver, protocol.name())
    {
        return Ok(None);
    }
    let headers_json =
        wire_shim_headers_snapshot(headers, driver, protocol, upstream_stream, responses_chat)?;
    Ok(Some(WireShimPlan {
        tenant_id: key.tenant_id,
        context: RequestContext {
            tenant_id: key.tenant_id.to_string(),
            principal_id: key.principal_id.to_string(),
            key_id: key.key_id.to_string(),
            protocol: protocol.name().to_owned(),
            model: model.to_owned(),
            // Replaced with the resolved plugin configuration at
            // materialization time (global default plus tenant override).
            config_json: "{}".to_owned(),
        },
        driver: driver.to_owned(),
        protocol: protocol.name().to_owned(),
        headers_json,
    }))
}

const MAX_WIRE_SHIM_HEADERS_SNAPSHOT_BYTES: usize = 128 * 1024;

/// Snapshot of the non-sensitive headers the host is about to send upstream.
/// Credential headers (authorization/x-api-key/cookie) never enter it; the
/// core-owned content-type/accept reflect the effective outbound values.
fn wire_shim_headers_snapshot(
    headers: &HeaderMap,
    driver: &str,
    protocol: Protocol,
    upstream_stream: bool,
    responses_chat: bool,
) -> Result<String, AppError> {
    let accept = if (responses_chat && upstream_stream)
        || (driver == crate::provider::CBCNX_PROVIDER_DRIVER
            && matches!(protocol, Protocol::OpenAiResponses)
            && upstream_stream)
    {
        "text/event-stream"
    } else {
        headers
            .get(header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/json")
    };
    let mut snapshot: std::collections::BTreeMap<String, String> = Default::default();
    for (name, value) in headers {
        let name = name.as_str();
        if matches!(name, "authorization" | "x-api-key" | "cookie") {
            continue;
        }
        let Ok(value) = value.to_str() else {
            continue;
        };
        snapshot
            .entry(name.to_owned())
            .and_modify(|existing: &mut String| {
                existing.push_str(", ");
                existing.push_str(value);
            })
            .or_insert_with(|| value.to_owned());
    }
    snapshot.insert("content-type".to_owned(), "application/json".to_owned());
    snapshot.insert("accept".to_owned(), accept.to_owned());
    let json = serde_json::to_string(&snapshot).map_err(|_| AppError::Internal)?;
    if json.len() > MAX_WIRE_SHIM_HEADERS_SNAPSHOT_BYTES {
        return Err(AppError::BadRequest(
            "request headers exceed the wire-shim snapshot limit".into(),
        ));
    }
    Ok(json)
}

pub(super) async fn materialize_proxy_route(
    state: &AppState,
    planned: PlannedProxyRoute,
) -> Result<PreparedProxyRoute, AppError> {
    let encoded_length = crate::gateway_body::memory::json_encoded_length(&planned.forwarded_json)?;
    // Allocate temporary serialization/adapter work before cloning any payload.
    let temporary_bytes = if planned.component_context.is_some() || planned.wire_shim.is_some() {
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
    // Post-serialization wire-shim hook. The returned body replaces the
    // serialized request byte-for-byte; the host never parses or re-encodes
    // it again. Any plugin error or allowlist violation rejects the request
    // (fail-closed). The hook ran once here, so retries of this prepared
    // route reuse the same body and plugin headers (a plugin-set
    // x-client-request-id stays stable across attempts).
    // Archive/idempotency ordering (design note): the archived request body
    // is frozen from the pre-hook canonical body during archive admission in
    // proxy.rs, which runs before materialization, so the shimmed bytes are
    // upstream-only and never alter archive or dedup semantics.
    let mut wire_shim_set_headers = None;
    if let Some(plan) = planned.wire_shim {
        // Run the hook and resolve configuration against the same runtime
        // that matched at plan time (pinned snapshot when revisions are
        // pinned, baseline otherwise), so a request never mixes revisions.
        let plugins = wire_shim_runtime(state);
        let configurations = plugins
            .resolved_traffic_configurations(plan.tenant_id)
            .await?;
        let body = String::from_utf8(forwarded_body).map_err(|_| AppError::Internal)?;
        let outcome = crate::api::plugin_execution::run(
            state.metrics.clone(),
            crate::api::plugin_execution::Phase::WireShimFinalize,
            move || {
                plugins.finalize_wire_shim(
                    &plan.driver,
                    &plan.protocol,
                    plan.context,
                    &body,
                    &plan.headers_json,
                    &configurations,
                )
            },
        )
        .await?;
        let Some(outcome) = outcome else {
            // The matched-plugin set is frozen at load time, so a plan implies
            // a hook; guard anyway rather than silently sending unshimmed.
            return Err(AppError::Internal);
        };
        let validated = http::validate_wire_shim_set_headers(
            &outcome.plugin_id,
            &outcome.set_headers,
        )?;
        wire_shim_set_headers = Some(validated);
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
        wire_shim_set_headers,
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
    http::send_reqwest_proxy_route(state, headers, protocol, request_id, route, outbound_attempt)
        .await
}

pub(super) fn retryable_upstream_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
    ) || status.is_server_error()
}

#[cfg(all(test, feature = "experimental-plugin-revisions"))]
mod wire_shim_revision_tests {
    use super::*;
    use crate::config::Config;
    use crate::plugin::application::{
        ApplicationPlugins, PreinstalledInventory, PublishApplicationPlugin,
    };
    use crate::plugin::lifecycle::{PluginGrant, manifest_digest};
    use crate::plugin::{PluginRuntime, memeloop::token_center::types::RequestContext};
    use std::collections::BTreeMap;

    /// The baseline runtime carries no plugins while the pinned application
    /// snapshot carries the real claude-code-wire component: matching,
    /// configuration resolution and the finalize hook must all come from the
    /// snapshot, so a published revision takes effect without a restart.
    #[tokio::test]
    async fn wire_shim_uses_pinned_application_snapshot_when_baseline_is_empty() {
        let directory = tempfile::tempdir().unwrap();
        let package = directory.path().join("claude-code-wire");
        std::fs::create_dir(&package).unwrap();
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/claude-code-wire");
        std::fs::copy(fixture.join("plugin.json"), package.join("plugin.json")).unwrap();
        std::fs::copy(fixture.join("plugin.wasm"), package.join("plugin.wasm")).unwrap();
        // Test-only host-approved provenance receipt, the same pattern the
        // application-authority tests use; no production verifier is bypassed.
        std::fs::write(
            package.join(".mtc-oci-install.json"),
            serde_json::to_vec(&serde_json::json!({
                "format_version": 1,
                "source": "ghcr.io/example/test-inventory",
                "digest": format!("sha256:{}", "a".repeat(64)),
                "signature_policy": "cosign-public-key"
            }))
            .unwrap(),
        )
        .unwrap();

        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("wire-shim.db").display()
        );
        let state = AppState::initialize(Config::for_test(database_url))
            .await
            .unwrap();
        // Baseline has no wire-shim plugin: planning must not match.
        assert!(!wire_shim_runtime(&state).wire_shim_matches("anthropic-claude", "anthropic"));

        let runtime = PluginRuntime::load(directory.path().to_str(), state.db.clone()).unwrap();
        let identities = runtime.package_identities();
        let grants = runtime
            .manifests()
            .into_iter()
            .map(|manifest| {
                let grant = PluginGrant {
                    version: manifest.version.clone(),
                    capabilities: manifest.capabilities.clone(),
                    manifest_digest: manifest_digest(&manifest).unwrap(),
                    identity: identities[&manifest.id].clone(),
                };
                (manifest.id, vec![grant])
            })
            .collect();
        let authority = std::sync::Arc::new(
            ApplicationPlugins::new(
                state.db.clone(),
                BTreeMap::from([(
                    "wire".to_owned(),
                    PreinstalledInventory {
                        root: directory.path().to_path_buf(),
                        grants,
                    },
                )]),
                &state.plugins,
            )
            .unwrap(),
        );
        authority
            .publish(
                PublishApplicationPlugin {
                    inventory_id: "wire".into(),
                    expected_revision: 0,
                },
                "wire-shim-pin",
            )
            .await
            .unwrap();
        let pinned = state
            .clone()
            .with_pinned_application_plugins(authority.pin().await.unwrap());

        let plugins = wire_shim_runtime(&pinned);
        assert!(plugins.wire_shim_matches("anthropic-claude", "anthropic"));
        assert!(!plugins.wire_shim_matches("anthropic-claude", "openai"));

        // Configuration resolves against the pinned snapshot's runtime and
        // the finalize hook emits the Claude Code wire format.
        let configurations = plugins
            .resolved_traffic_configurations(uuid::Uuid::new_v4())
            .await
            .unwrap();
        let context = RequestContext {
            tenant_id: "tenant".to_owned(),
            principal_id: "principal".to_owned(),
            key_id: "key".to_owned(),
            protocol: "anthropic".to_owned(),
            model: "claude-sonnet-4-5".to_owned(),
            config_json: "{}".to_owned(),
        };
        let outcome = plugins
            .finalize_wire_shim(
                "anthropic-claude",
                "anthropic",
                context,
                r#"{"model":"claude-sonnet-4-5","max_tokens":64,"messages":[{"role":"user","content":"hello world, this is a prompt"}]}"#,
                "{}",
                &configurations,
            )
            .unwrap()
            .expect("pinned snapshot applies the wire shim");
        assert!(outcome.request_json.contains("x-anthropic-billing-header"));
    }
}
