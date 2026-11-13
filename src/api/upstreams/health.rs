use super::super::*;
use super::probe_model::{CatalogModel, select_probe_model};

const PROBE_INSTRUCTIONS: &str = "Reply with ok.";
const HEALTHY_INTERVAL: i64 = 3_600_000;
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct HealthProbeSettings {
    pub enabled: bool,
    pub billing_key_id: Option<Uuid>,
    pub interval_seconds: i64,
    pub model: Option<String>,
    pub prompt: String,
}

impl Default for HealthProbeSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            billing_key_id: None,
            interval_seconds: 3600,
            model: None,
            prompt: PROBE_INSTRUCTIONS.into(),
        }
    }
}

fn periodic_probe_due(settings: &HealthProbeSettings, last: Option<i64>, now: i64) -> bool {
    settings.enabled
        && last
            .is_none_or(|v| now.saturating_sub(v) >= settings.interval_seconds.saturating_mul(1000))
}

async fn load_probe_settings(
    state: &AppState,
    account_id: Uuid,
) -> Result<HealthProbeSettings, AppError> {
    match state
        .db
        .plugin_kv_get("upstream-health-probe", &account_id.to_string())
        .await?
    {
        Some(value) => {
            serde_json::from_slice(&value).map_err(|_| AppError::internal("health_probe_settings"))
        }
        None => Ok(HealthProbeSettings::default()),
    }
}

async fn authorize_probe_settings(
    state: &AppState,
    headers: &HeaderMap,
    tenant: Option<&str>,
    account_id: Uuid,
    scope: &str,
) -> Result<(), AppError> {
    let service = require_service(headers, state, scope).await?;
    if let Some(tenant) = tenant.or(service.tenant_external_id.as_deref()) {
        require_service_tenant(&service, tenant)?;
        state.db.require_upstream_tenant(account_id, tenant).await?;
    }
    state.db.probe_account_context(account_id).await?;
    Ok(())
}

pub(in crate::api) async fn get_health_probe_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Query(query): Query<ManagementTenantQuery>,
) -> Result<Json<HealthProbeSettings>, AppError> {
    authorize_probe_settings(
        &state,
        &headers,
        query.tenant_external_id.as_deref(),
        account_id,
        "providers:read",
    )
    .await?;
    Ok(Json(load_probe_settings(&state, account_id).await?))
}

pub(in crate::api) async fn put_health_probe_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Query(query): Query<ManagementTenantQuery>,
    Json(settings): Json<HealthProbeSettings>,
) -> Result<Json<HealthProbeSettings>, AppError> {
    authorize_probe_settings(
        &state,
        &headers,
        query.tenant_external_id.as_deref(),
        account_id,
        "providers:write",
    )
    .await?;
    require_service(&headers, &state, "keys:write").await?;
    if settings.interval_seconds < 3600
        || settings.interval_seconds > 2_592_000
        || settings.prompt.trim().is_empty()
        || settings.prompt.len() > 1024
        || settings
            .model
            .as_ref()
            .is_some_and(|v| v.trim().is_empty() || v.len() > 200)
        || (settings.enabled && settings.billing_key_id.is_none())
    {
        return Err(AppError::BadRequest("invalid health probe settings".into()));
    }
    let account = state.db.probe_account_context(account_id).await?;
    if let Some(key) = settings.billing_key_id {
        state
            .db
            .filter_assistant_identity(&account.tenant_external_id, key)
            .await?;
        if super::super::control_requests::load_filter_assistant_settings(
            &state,
            &account.tenant_external_id,
        )
        .await?
        .and_then(|s| s.billing_key_id)
            == Some(key)
        {
            return Err(AppError::BadRequest(
                "health probe and system AI must use different credentials".into(),
            ));
        }
    }
    let value =
        serde_json::to_vec(&settings).map_err(|_| AppError::internal("health_probe_settings"))?;
    state
        .db
        .plugin_kv_put("upstream-health-probe", &account_id.to_string(), &value)
        .await?;
    Ok(Json(settings))
}

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
        AppError::Capacity { .. } => ProbeDisposition::Skip("probe_admission_skipped"),
        AppError::Overloaded => ProbeDisposition::Skip("service_overloaded"),
        AppError::Internal { .. } | AppError::Storage { .. } => {
            ProbeDisposition::Skip("probe_state_unavailable")
        }
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
            .map(visible_probe_attempt)
            .unwrap_or(Value::Null),
    ))
}

/// Existing clients render the top-level status. A skipped latest attempt
/// must remain visible after reload, without erasing the last real result.
fn visible_probe_attempt(mut result: Value) -> Value {
    let Some(attempt) = result
        .get("last_attempt")
        .filter(|attempt| attempt["status"] == "skipped")
    else {
        return result;
    };
    let mut visible = attempt.clone();
    result
        .as_object_mut()
        .expect("probe results are objects")
        .remove("last_attempt");
    visible["last_result"] = if result["status"] == "skipped" {
        Value::Null
    } else {
        result
    };
    visible
}

pub(crate) async fn run_upstream_service_probes(
    state: crate::AppState,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut ticks = tokio::time::interval(Duration::from_secs(60));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! { biased;
            _ = crate::worker::wait_for_shutdown(&mut shutdown) => return,
            _ = ticks.tick() => {}
        }
        let now = unix_millis();
        let due = match state
            .db
            .configured_upstream_service_probes_due(now.saturating_sub(HEALTHY_INTERVAL), 10_000)
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
            let Ok(settings) = load_probe_settings(&state, account_id).await else {
                continue;
            };
            let Ok(last) = state
                .db
                .upstream_service_probe_last_attempt(account_id)
                .await
            else {
                continue;
            };
            if !periodic_probe_due(&settings, last, unix_millis()) {
                continue;
            }
            if inflight.len() >= 32 {
                break;
            }
            match state
                .db
                .claim_upstream_service_probe(
                    account_id,
                    unix_millis(),
                    settings.interval_seconds.saturating_mul(1000),
                )
                .await
            {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    tracing::warn!(
                        %account_id,
                        error_category = error.diagnostic_category(),
                        "upstream service probe claim failed"
                    );
                    continue;
                }
            }
            let state = state.clone();
            inflight.spawn(async move {
                // At most one request per account per interval. Rotate scoped
                // quota recovery models; a probe of a different model cannot
                // prove that the exhausted model recovered.
                let recovery = match state.db.supplier_quota_recovery_models(account_id).await {
                    Ok(models) if !models.is_empty() => {
                        let slot = (unix_millis() / settings.interval_seconds.saturating_mul(1000))
                            as usize;
                        Some(models[slot % models.len()].clone())
                    }
                    Ok(_) => None,
                    Err(error) => {
                        tracing::warn!(
                            error_category = error.diagnostic_category(),
                            "supplier quota recovery schedule failed"
                        );
                        return;
                    }
                };
                let trigger = if recovery.is_some() {
                    "periodic_recovery"
                } else {
                    "periodic"
                };
                run_upstream_service_probe(&state, account_id, trigger, recovery.as_deref()).await;
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

pub(in crate::api) async fn run_upstream_service_probe(
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

fn probe_envelope(protocol: &str, public_model: &str, prompt: &str) -> Option<(Protocol, Value)> {
    match protocol {
        "openai" => Some((
            Protocol::OpenAiResponses,
            json!({
                "model": public_model,
                "instructions": prompt,
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
                "system": prompt,
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
    let settings = load_probe_settings(state, account_id)
        .await
        .map_err(|error| prepare_failed(account_id, &error, "probe_state_unavailable"))?;
    let configured = recovery_model.or(settings.model.as_deref());
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
    let settings = match load_probe_settings(state, account_id).await {
        Ok(settings) => settings,
        Err(error) => {
            return Err(prepare_failed(
                account_id,
                &error,
                "probe_state_unavailable",
            ));
        }
    };
    let Some(billing_key_id) = settings.billing_key_id else {
        return Err(skipped(account_id, "probe_billing_unavailable"));
    };
    match super::super::control_requests::load_filter_assistant_settings(state, tenant).await {
        Ok(Some(system)) if system.billing_key_id == Some(billing_key_id) => {
            return Err(skipped(account_id, "probe_billing_unavailable"));
        }
        Err(error) => {
            return Err(prepare_failed(
                account_id,
                &error,
                "probe_state_unavailable",
            ));
        }
        _ => {}
    }
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
    // Feasibility and ordering use the same currency and exact model as
    // forwarding. Missing prices are local skips, never discovery POSTs.
    let mut billable = Vec::new();
    for mut candidate in catalog {
        match state
            .db
            .model_price(&candidate.model_id, &key.currency)
            .await
        {
            Ok(price) => {
                // Rank by the estimated price of a small probe, including output.
                // The prompt is bounded and the response is capped at 16 tokens.
                candidate.input_price = Some(
                    price
                        .input_micros_per_million
                        .saturating_mul(64)
                        .saturating_add(price.output_micros_per_million.saturating_mul(16)),
                );
                billable.push(candidate);
            }
            Err(AppError::UnpricedModel) => {
                if configured.map(str::trim) == Some(candidate.model_id.as_str()) {
                    let (mut body, store) = skipped(account_id, "probe_model_unpriced");
                    body["selected_model"] = json!(candidate.model_id);
                    return Err((body, store));
                }
            }
            Err(error) => {
                return Err(prepare_failed(
                    account_id,
                    &error,
                    "probe_state_unavailable",
                ));
            }
        }
    }
    catalog = billable;
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
    let Some((protocol, request)) =
        probe_envelope(&route.protocol, &route.public_model, &settings.prompt)
    else {
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
        body["status"] = json!("unhealthy");
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
    fn automatic_probe_can_be_disabled_and_waits_at_least_one_hour() {
        let mut settings = super::HealthProbeSettings::default();
        assert!(!super::periodic_probe_due(&settings, None, 10_000_000));
        settings.enabled = true;
        assert!(!super::periodic_probe_due(
            &settings,
            Some(1_000_000),
            4_599_999
        ));
        assert!(super::periodic_probe_due(
            &settings,
            Some(1_000_000),
            4_600_000
        ));
        settings.interval_seconds = 7200;
        assert!(!super::periodic_probe_due(
            &settings,
            Some(1_000_000),
            4_600_000
        ));
    }

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
