use super::super::*;

fn health_probe_error(driver: &str, status: StatusCode) -> Option<&'static str> {
    match status {
        StatusCode::TOO_MANY_REQUESTS => Some("rate_limited"),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Some("authentication_failed"),
        // This provider probes a deliberately nonexistent task without creating
        // a billable generation. Its authenticated not-found reply is expected.
        StatusCode::NOT_FOUND if driver == "volcengine-seedance" => None,
        status if status.is_success() => None,
        _ => Some("upstream_unavailable"),
    }
}

fn health_probe_transport_error(
    is_connect: bool,
    is_timeout: bool,
    proxy_configured: bool,
) -> &'static str {
    if is_connect && !is_timeout && !proxy_configured {
        "connection_failed"
    } else {
        "upstream_unavailable"
    }
}

fn upstream_health_probe_url(driver: &str, config: &Value, base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    match driver {
        "openai-codex" => {
            format!(
                "{base}/models?client_version={}",
                crate::oauth::managed::codex::CLIENT_VERSION
            )
        }
        driver
            if crate::provider::is_openai_compatible_http_driver(driver)
                || crate::provider::is_new_api_driver(driver) =>
        {
            if base.ends_with("/v1") {
                format!("{base}/models")
            } else {
                format!("{base}/v1/models")
            }
        }
        "comfyui" => {
            let prefix = config
                .get("api_prefix")
                .and_then(Value::as_str)
                .unwrap_or_default();
            format!("{base}{prefix}/system_stats")
        }
        "volcengine-seedance" => {
            format!("{base}/api/v3/contents/generations/tasks/__mtc_health_probe__")
        }
        _ => base.to_owned(),
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
    Ok(Json(run_upstream_service_probe(&state, account_id).await))
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
            .upstream_service_probes_due(now.saturating_sub(60_000), 4)
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
        for account_id in due {
            tokio::select! { biased;
                _ = crate::worker::wait_for_shutdown(&mut shutdown) => return,
                _ = run_upstream_service_probe(&state, account_id) => {}
            }
        }
    }
}

async fn run_upstream_service_probe(state: &AppState, account_id: Uuid) -> Value {
    let body = upstream_service_probe_body(state, account_id).await;
    if let Err(error) = state
        .db
        .store_upstream_service_probe(account_id, &body)
        .await
    {
        tracing::warn!(%account_id, error_category = error.diagnostic_category(), "upstream service probe result was not stored");
    }
    body
}

async fn upstream_service_probe_body(state: &AppState, account_id: Uuid) -> Value {
    let account_driver = match state.db.upstream_driver(account_id).await {
        Ok(driver) => driver,
        Err(_) => {
            return json!({"account_id": account_id, "status": "unhealthy", "error_code": "health_state_unavailable", "source": "connection_probe", "checked_at": unix_millis()});
        }
    };
    if !state.providers.is_public(&account_driver) {
        return json!({
            "account_id": account_id,
            "status": "unhealthy",
            "error_code": "provider_retired",
            "checked_at": unix_millis()
        });
    }
    let (account, credential) = match state
        .db
        .upstream_account_with_credential(account_id, state.config.key_pepper.as_bytes())
        .await
    {
        Ok(loaded) => loaded,
        Err(_) => {
            return json!({
                "account_id": account_id,
                "status": "unhealthy",
                "error_code": "health_state_unavailable",
                "source": "connection_probe",
                "checked_at": unix_millis()
            });
        }
    };
    if credential.validate(unix_millis()).is_err() {
        return json!({
            "account_id": account_id,
            "status": "unhealthy",
            "error_code": "credential_invalid",
            "checked_at": unix_millis()
        });
    }
    let credential = if account.driver == "openai-codex"
        || crate::oauth::authorization_code::supports_managed_transport(&credential)
    {
        match state.transport_proxy_groups.select_config(
            account.id,
            account.credential_generation,
            &credential,
            &account.config,
        ) {
            Ok(selected) => selected.credential,
            Err(_) => {
                return json!({
                    "account_id": account_id,
                    "status": "unhealthy",
                    "error_code": "destination_invalid",
                    "source": "connection_probe",
                    "checked_at": unix_millis()
                });
            }
        }
    } else {
        credential
    };
    let proxy_configured = credential.proxy().is_some();
    let base_url = match validate_config(&account.config) {
        Ok(base_url) => base_url,
        Err(_) => {
            return json!({
                "account_id": account_id,
                "status": "unhealthy",
                "error_code": "destination_invalid",
                "checked_at": unix_millis()
            });
        }
    };
    let outbound = match if account.driver == "openai-codex" {
        network::client_for_codex_url(
            &state.http,
            &base_url,
            &account.config,
            credential.proxy(),
            state.config.codex_test_loopback,
        )
        .await
    } else {
        network::client_for_config_url(
            &state.http,
            &base_url,
            &account.config,
            credential.proxy(),
            state.config.allow_oauth_loopback,
        )
        .await
    } {
        Ok(client) => client,
        Err(_) => {
            return json!({
                "account_id": account_id,
                "status": "unhealthy",
                "error_code": "destination_invalid",
                "checked_at": unix_millis()
            });
        }
    };
    let probe_url = upstream_health_probe_url(&account.driver, &account.config, &base_url);
    let started = Instant::now();
    let request = outbound
        .get(probe_url)
        .header(header::ACCEPT, "application/json")
        .timeout(Duration::from_secs(30));
    let request = if account.driver == "openai-codex" {
        let account_id = match crate::oauth::managed::codex::account_header_value(&credential) {
            Ok(account_id) => account_id,
            Err(_) => {
                return json!({
                    "account_id": account_id,
                    "status": "unhealthy",
                    "error_code": "credential_invalid",
                    "checked_at": unix_millis()
                });
            }
        };
        request
            .header(header::USER_AGENT, crate::oauth::managed::codex::USER_AGENT)
            .header(header::CONNECTION, "Keep-Alive")
            .header("originator", crate::oauth::managed::codex::ORIGINATOR)
            .header("chatgpt-account-id", account_id)
    } else {
        request
    };
    let request = match credential.apply(request, unix_millis()) {
        Ok(request) => request,
        Err(_) => {
            return json!({
                "account_id": account_id,
                "status": "unhealthy",
                "error_code": "credential_invalid",
                "checked_at": unix_millis()
            });
        }
    };
    let response = request.send().await;
    let latency_ms = started.elapsed().as_millis().min(i64::MAX as u128) as i64;
    let checked_at = unix_millis();
    match response {
        Ok(response) => {
            let upstream_status = response.status();
            let error_code = if matches!(
                upstream_status,
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ) {
                Some(super::rejections::classify_auth_rejection(response).await)
            } else {
                health_probe_error(&account.driver, upstream_status)
            };
            let healthy = error_code.is_none();
            json!({
                "account_id": account_id,
                "status": if healthy { "healthy" } else { "unhealthy" },
                "error_code": error_code,
                "source": "connection_probe",
                "upstream_status": upstream_status.as_u16(),
                "latency_ms": latency_ms,
                "checked_at": checked_at
            })
        }
        Err(error) => json!({
            "account_id": account_id,
            "status": "unhealthy",
            "error_code": health_probe_transport_error(
                error.is_connect(),
                error.is_timeout(),
                proxy_configured,
            ),
            "latency_ms": latency_ms,
            "checked_at": checked_at
        }),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{health_probe_transport_error, upstream_health_probe_url};

    #[test]
    fn probe_transport_error_only_reports_verified_direct_connection_failures() {
        assert_eq!(
            health_probe_transport_error(true, false, false),
            "connection_failed"
        );
        for (is_connect, is_timeout, proxy_configured) in [
            (true, false, true),
            (true, true, false),
            (true, true, true),
            (false, true, false),
            (false, true, true),
            (false, false, false),
            (false, false, true),
        ] {
            assert_eq!(
                health_probe_transport_error(is_connect, is_timeout, proxy_configured),
                "upstream_unavailable"
            );
        }
    }

    #[test]
    fn probe_rejections_are_not_reported_as_healthy() {
        use super::{StatusCode, health_probe_error};
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
                health_probe_error("openai-codex", StatusCode::from_u16(status).unwrap()),
                expected
            );
        }
        assert_eq!(
            health_probe_error("volcengine-seedance", StatusCode::NOT_FOUND),
            None
        );
        assert_eq!(
            health_probe_error("volcengine-seedance", StatusCode::TOO_MANY_REQUESTS),
            Some("rate_limited")
        );
    }

    #[test]
    fn codex_health_uses_the_authenticated_model_catalog_endpoint() {
        let url = upstream_health_probe_url(
            "openai-codex",
            &json!({}),
            "https://chatgpt.com/backend-api/codex/",
        );
        assert_eq!(
            url,
            format!(
                "https://chatgpt.com/backend-api/codex/models?client_version={}",
                crate::oauth::managed::codex::CLIENT_VERSION
            )
        );
    }

    #[test]
    fn http_json_health_uses_the_bounded_openai_model_catalog_endpoint() {
        assert_eq!(
            upstream_health_probe_url("http-json", &json!({}), "https://upstream.example.test/v1/",),
            "https://upstream.example.test/v1/models",
        );
    }

    #[test]
    fn new_api_health_uses_the_bounded_openai_model_catalog_endpoint() {
        assert_eq!(
            upstream_health_probe_url("new-api", &json!({}), "https://zero.cat/"),
            "https://zero.cat/v1/models",
        );
    }
}
