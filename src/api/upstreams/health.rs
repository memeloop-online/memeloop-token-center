use super::super::*;
use super::probe_model::{CatalogModel, select_probe_model};

const PROBE_INSTRUCTIONS: &str = "Reply with ok.";
const HEALTHY_INTERVAL: i64 = 60_000;
const UNHEALTHY_INTERVAL: i64 = 10_000;
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

fn health_probe_error(status: StatusCode) -> Option<&'static str> {
    match status {
        StatusCode::TOO_MANY_REQUESTS => Some("rate_limited"),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Some("authentication_failed"),
        status if status.is_success() => None,
        _ => Some("upstream_unavailable"),
    }
}

enum ProbeDisposition {
    Store(&'static str),
    Skip(&'static str),
}

fn classify_probe_error(error: &AppError) -> ProbeDisposition {
    match error {
        AppError::Upstream { .. } => ProbeDisposition::Store("upstream_unavailable"),
        AppError::OAuthRefresh(_) => ProbeDisposition::Store("authentication_failed"),
        AppError::Capacity { code, .. } => ProbeDisposition::Skip(code),
        AppError::Overloaded => ProbeDisposition::Skip("service_overloaded"),
        AppError::Internal { code, .. } => ProbeDisposition::Skip(code),
        AppError::Storage { code, .. } => ProbeDisposition::Skip(code),
        AppError::Forbidden
        | AppError::NoAuthorizedUpstreamCandidate
        | AppError::Unauthorized
        | AppError::BadRequest(_)
        | AppError::QuotaExceeded
        | AppError::UnpricedModel
        | AppError::RateLimited
        | AppError::LimitExceeded { .. }
        | AppError::NotFound
        | AppError::Conflict(_)
        | AppError::ProxyGroupConflict(_)
        | AppError::CompactionProducerUnavailable
        | AppError::PluginInputTooLarge
        | AppError::DatabaseUnavailable(_)
        | AppError::SchemaSecretAnalysisTooComplex
        | AppError::PluginReleaseDownload(_) => ProbeDisposition::Skip("probe_admission_skipped"),
    }
}

pub(in crate::api) async fn probe_upstream_health(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Query(query): Query<ManagementTenantQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:write").await?;
    let state = state.pin_application_plugins().await?;
    if let Some(tenant) = query
        .tenant_external_id
        .as_deref()
        .or(service.tenant_external_id.as_deref())
    {
        require_service_tenant(&service, tenant)?;
        state.db.require_upstream_tenant(account_id, tenant).await?;
    }
    Ok(Json(
        run_upstream_service_probe(&state, account_id, "manual", None).await,
    ))
}

pub(in crate::api) async fn read_upstream_service_probe(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Query(query): Query<ManagementTenantQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:read").await?;
    if let Some(tenant) = query
        .tenant_external_id
        .as_deref()
        .or(service.tenant_external_id.as_deref())
    {
        require_service_tenant(&service, tenant)?;
        state.db.require_upstream_tenant(account_id, tenant).await?;
    }
    Ok(Json(
        state
            .db
            .upstream_service_probe(account_id)
            .await?
            .unwrap_or(Value::Null),
    ))
}

pub(crate) async fn run_upstream_service_probes(
    state: crate::AppState,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut ticks = tokio::time::interval(Duration::from_secs(10));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! { biased;
            _ = crate::worker::wait_for_shutdown(&mut shutdown) => return,
            _ = ticks.tick() => {}
        }
        let now = unix_millis();
        let due = match state
            .db
            .upstream_service_probes_due(
                now.saturating_sub(HEALTHY_INTERVAL),
                now.saturating_sub(UNHEALTHY_INTERVAL),
                32,
            )
            .await
        {
            Ok(due) => due,
            Err(error) => {
                tracing::warn!(
                    error_category = error.diagnostic_category(),
                    "upstream service probe schedule failed"
                );
                continue;
            }
        };
        let mut inflight = tokio::task::JoinSet::new();
        for account_id in due {
            let state = state.clone();
            inflight.spawn(async move {
                let result = run_upstream_service_probe(&state, account_id, "periodic", None).await;
                // Recovery remains background and pinned; a cheap default
                // success cannot establish availability for another model.
                match state.db.supplier_quota_recovery_models(account_id).await {
                    Ok(models) => {
                        for model in models {
                            if result["selected_model"].as_str() != Some(&model) {
                                run_upstream_service_probe(
                                    &state,
                                    account_id,
                                    "periodic_recovery",
                                    Some(&model),
                                )
                                .await;
                            }
                        }
                    }
                    Err(error) => tracing::warn!(
                        error_category = error.diagnostic_category(),
                        "supplier quota recovery schedule failed"
                    ),
                }
            });
        }
        while !inflight.is_empty() {
            tokio::select! { biased;
                _ = crate::worker::wait_for_shutdown(&mut shutdown) => {
                    inflight.abort_all();
                    return;
                }
                joined = inflight.join_next() => {
                    if joined.is_none() {
                        break;
                    }
                }
            }
        }
    }
}

async fn run_upstream_service_probe(
    state: &AppState,
    account_id: Uuid,
    trigger: &str,
    recovery_model: Option<&str>,
) -> Value {
    let account = match state.db.probe_account_context(account_id).await {
        Ok(account) => account,
        Err(_) => return skipped(account_id, "probe_account_unavailable").0,
    };
    let started_at = unix_millis();
    let (mut body, _) = upstream_service_probe_body(
        state,
        account_id,
        &account,
        recovery_model,
        trigger != "manual",
    )
    .await;
    body["trigger"] = json!(trigger);
    body["started_at"] = json!(started_at);
    body["credential_generation"] = json!(account.credential_generation);
    body["transport_revision"] = json!(account.transport_revision);
    if let Err(error) = state
        .db
        .store_upstream_service_probe(account_id, &body)
        .await
    {
        tracing::warn!(%account_id, error_category = error.diagnostic_category(), "upstream service probe result was not stored");
    }
    body
}

fn skipped(account_id: Uuid, error_code: &'static str) -> (Value, bool) {
    (
        json!({
            "account_id": account_id,
            "status": "skipped",
            "error_code": error_code,
            "source": "model_probe",
            "checked_at": unix_millis()
        }),
        true,
    )
}

fn recorded(
    account_id: Uuid,
    healthy: bool,
    error_code: Option<&str>,
    latency_ms: Option<i64>,
    upstream_status: Option<u16>,
) -> (Value, bool) {
    (
        json!({
            "account_id": account_id,
            "status": if healthy { "healthy" } else { "unhealthy" },
            "error_code": error_code,
            "source": "model_probe",
            "upstream_status": upstream_status,
            "latency_ms": latency_ms,
            "checked_at": unix_millis()
        }),
        true,
    )
}

struct PreparedProbe {
    key: crate::model::AuthenticatedKey,
    route_id: Uuid,
    protocol: Protocol,
    body: Vec<u8>,
    model: String,
    credential_generation: i64,
    transport_revision: i64,
}

fn probe_envelope(protocol: &str, public_model: &str) -> Option<(Protocol, Value)> {
    match protocol {
        "openai" => Some((
            Protocol::OpenAiResponses,
            json!({
                "model": public_model,
                "instructions": PROBE_INSTRUCTIONS,
                "input": "hi",
                "max_output_tokens": 16,
                "stream": false,
                "store": false
            }),
        )),
        "anthropic" => Some((
            Protocol::AnthropicMessages,
            json!({
                "model": public_model,
                "system": PROBE_INSTRUCTIONS,
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 16,
                "stream": false
            }),
        )),
        _ => None,
    }
}

fn prepare_failed(account_id: Uuid, error: &AppError, code: &'static str) -> (Value, bool) {
    tracing::warn!(
        %account_id,
        error_category = error.diagnostic_category(),
        "upstream service probe was not prepared"
    );
    skipped(account_id, code)
}

async fn prepare_model_probe(
    state: &AppState,
    account_id: Uuid,
    account: &crate::db::upstream_service_probes::ProbeAccountContext,
    recovery_model: Option<&str>,
) -> Result<PreparedProbe, (Value, bool)> {
    if !state.providers.is_public(&account.driver) {
        return Err(skipped(account_id, "provider_retired"));
    }
    let text_provider = state
        .providers
        .get(&account.driver)
        .is_some_and(|provider| {
            provider
                .modalities
                .iter()
                .any(|modality| modality == "text")
        });
    if !text_provider {
        return Err(skipped(account_id, "probe_model_unavailable"));
    }
    let catalog = match state.db.probe_catalog_models(account_id).await {
        Ok(rows) => rows
            .into_iter()
            .map(|row| CatalogModel {
                model_id: row.model_id,
                protocol: row.protocol,
                input_price: row.input_price,
            })
            .collect::<Vec<_>>(),
        Err(error) => {
            return Err(prepare_failed(
                account_id,
                &error,
                "probe_state_unavailable",
            ));
        }
    };
    let configured =
        recovery_model.or_else(|| account.config.get("probe_model").and_then(Value::as_str));
    if select_probe_model(configured, &catalog).is_none() {
        return Err(skipped(account_id, "probe_model_unavailable"));
    }
    bill_model_probe(state, account_id, account, configured, catalog).await
}

async fn bill_model_probe(
    state: &AppState,
    account_id: Uuid,
    account: &crate::db::upstream_service_probes::ProbeAccountContext,
    configured: Option<&str>,
    mut catalog: Vec<CatalogModel>,
) -> Result<PreparedProbe, (Value, bool)> {
    let tenant = &account.tenant_external_id;
    let settings =
        match super::super::control_requests::load_filter_assistant_settings(state, tenant).await {
            Ok(settings) => settings,
            Err(error) => {
                return Err(prepare_failed(
                    account_id,
                    &error,
                    "probe_state_unavailable",
                ));
            }
        };
    let Some(billing_key_id) = settings.and_then(|settings| settings.billing_key_id) else {
        return Err(skipped(account_id, "probe_billing_unavailable"));
    };
    let key = match state
        .db
        .filter_assistant_identity(tenant, billing_key_id)
        .await
    {
        Ok(key) => key,
        Err(error) => {
            return Err(prepare_failed(
                account_id,
                &error,
                "probe_billing_unavailable",
            ));
        }
    };
    // Remove unroutable models before applying the established price/version/
    // list ordering. Explicit selections never silently fall back.
    let (model, route) = loop {
        let Some(model) = select_probe_model(configured, &catalog) else {
            return Err(skipped(account_id, "probe_route_unavailable"));
        };
        match state
            .db
            .probe_billing_route(account_id, key.key_id, &model)
            .await
        {
            Ok(Some(route)) => break (model, route),
            Ok(None)
                if configured
                    .map(str::trim)
                    .is_some_and(|value| !value.is_empty()) =>
            {
                let (mut body, store) = skipped(account_id, "probe_route_unavailable");
                body["selected_model"] = json!(model);
                return Err((body, store));
            }
            Ok(None) => catalog.retain(|candidate| candidate.model_id != model),
            Err(error) => {
                return Err(prepare_failed(
                    account_id,
                    &error,
                    "probe_state_unavailable",
                ));
            }
        }
    };
    let Some((protocol, request)) = probe_envelope(&route.protocol, &route.public_model) else {
        return Err(skipped(account_id, "probe_route_unavailable"));
    };
    let body =
        serde_json::to_vec(&request).map_err(|_| skipped(account_id, "probe_state_unavailable"))?;
    Ok(PreparedProbe {
        key,
        route_id: route.route_id,
        protocol,
        body,
        model,
        credential_generation: account.credential_generation,
        transport_revision: account.transport_revision,
    })
}

async fn send_model_probe(
    state: &AppState,
    account_id: Uuid,
    prepared: PreparedProbe,
) -> (Value, bool) {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-mtc-client-name",
        HeaderValue::from_static("upstream-health-probe"),
    );
    let started = Instant::now();
    let context = super::super::proxy_diagnostics::Context::new();
    let target = super::super::proxy::IndependentProbeTarget::new(
        account_id,
        prepared.model.clone(),
        prepared.credential_generation,
        prepared.transport_revision,
    );
    let memory = state.proxy_memory_budget.reservation();
    let dispatched = tokio::time::timeout(
        PROBE_TIMEOUT,
        super::super::proxy_diagnostics::CONTEXT.scope(
            context,
            super::super::proxy::proxy_with_pinned_account(
                state.clone(),
                headers,
                Bytes::from(prepared.body),
                prepared.protocol,
                prepared.key,
                prepared.route_id,
                account_id,
                memory,
                target.clone(),
            ),
        ),
    )
    .await;
    let latency_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let (mut body, store) = match dispatched {
        Err(_) => recorded(
            account_id,
            false,
            Some("upstream_unavailable"),
            Some(latency_ms),
            None,
        ),
        Ok(Err(error)) => match classify_probe_error(&error) {
            ProbeDisposition::Skip(code) => {
                tracing::info!(
                    %account_id,
                    error_category = error.diagnostic_category(),
                    "upstream service probe was not billed"
                );
                skipped(account_id, code)
            }
            ProbeDisposition::Store(code) => {
                recorded(account_id, false, Some(code), Some(latency_ms), None)
            }
        },
        Ok(Ok(response)) => {
            let status = response.status();
            drop(response);
            let code = health_probe_error(status);
            recorded(
                account_id,
                code.is_none(),
                code,
                Some(latency_ms),
                Some(status.as_u16()),
            )
        }
    };
    body["request_id"] = json!(context.request_id);
    body["selected_model"] = json!(prepared.model);
    let observation = target.observation();
    body["actual_model"] = json!(observation.model);
    body["upstream_status"] = json!(observation.status);
    if body["status"] != "skipped" && observation.status.is_none() && observation.model.is_none() {
        body["status"] = json!("skipped");
        body["error_code"] = json!("probe_admission_skipped");
    }
    if let Some(quota) = observation.quota {
        body["error_code"] = json!(if quota.exhausted {
            "quota_exhausted"
        } else {
            "rate_limited"
        });
        body["reset_at"] = json!(quota.reset_at);
        body["reset_source"] = json!(quota.reset_source);
    }
    body["status_origin"] = json!(if observation.status.is_some() {
        "provider"
    } else {
        "mtc"
    });
    (body, store)
}

async fn upstream_service_probe_body(
    state: &AppState,
    account_id: Uuid,
    account: &crate::db::upstream_service_probes::ProbeAccountContext,
    recovery_model: Option<&str>,
    periodic: bool,
) -> (Value, bool) {
    match prepare_model_probe(state, account_id, account, recovery_model).await {
        Ok(prepared) => {
            if periodic {
                match state
                    .db
                    .supplier_quota_probe_deadline(account_id, &prepared.model)
                    .await
                {
                    Ok(Some(deadline)) => {
                        let (mut body, store) = skipped(account_id, "supplier_quota_reset_pending");
                        body["selected_model"] = json!(prepared.model);
                        body["reset_at"] = json!(deadline);
                        return (body, store);
                    }
                    Err(error) => {
                        return prepare_failed(account_id, &error, "probe_state_unavailable");
                    }
                    Ok(None) => {}
                }
            }
            send_model_probe(state, account_id, prepared).await
        }
        Err(finished) => finished,
    }
}

#[cfg(test)]
mod tests {
    use super::{StatusCode, classify_probe_error, health_probe_error};
    use crate::error::AppError;

    #[test]
    fn probe_rejections_are_not_reported_as_healthy() {
        for (status, expected) in [
            (200, None),
            (302, Some("upstream_unavailable")),
            (400, Some("upstream_unavailable")),
            (401, Some("authentication_failed")),
            (403, Some("authentication_failed")),
            (404, Some("upstream_unavailable")),
            (429, Some("rate_limited")),
            (503, Some("upstream_unavailable")),
        ] {
            assert_eq!(
                health_probe_error(StatusCode::from_u16(status).unwrap()),
                expected
            );
        }
    }

    #[test]
    fn admission_gaps_are_not_stored_as_upstream_failures() {
        assert!(matches!(
            classify_probe_error(&AppError::NoAuthorizedUpstreamCandidate),
            super::ProbeDisposition::Skip(_)
        ));
        assert!(matches!(
            classify_probe_error(&AppError::Forbidden),
            super::ProbeDisposition::Skip(_)
        ));
        assert!(matches!(
            classify_probe_error(&AppError::Overloaded),
            super::ProbeDisposition::Skip(_)
        ));
        assert!(matches!(
            classify_probe_error(&AppError::QuotaExceeded),
            super::ProbeDisposition::Skip(_)
        ));
        assert!(matches!(
            classify_probe_error(&AppError::upstream_with(
                "upstream_unavailable",
                crate::error::ErrorFacts::new()
                    .stage("health_admission_gaps_are_not_stored_as_upstream_failures")
            )),
            super::ProbeDisposition::Store("upstream_unavailable")
        ));
    }
}
