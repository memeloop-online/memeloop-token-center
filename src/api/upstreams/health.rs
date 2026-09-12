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

fn upstream_health_probe_url(driver: &str, config: &Value, base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    match driver {
        "openai-codex" => {
            format!(
                "{base}/models?client_version={}",
                crate::oauth::managed::codex::CLIENT_VERSION
            )
        }
        driver if crate::provider::is_openai_compatible_http_driver(driver) => {
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
    if let Some(tenant) = query
        .tenant_external_id
        .as_deref()
        .or(service.tenant_external_id.as_deref())
    {
        require_service_tenant(&service, tenant)?;
        state.db.require_upstream_tenant(account_id, tenant).await?;
    }
    let account_driver = state.db.upstream_driver(account_id).await?;
    if !state.providers.is_public(&account_driver) {
        return Ok(Json(json!({
            "account_id": account_id,
            "status": "unhealthy",
            "error_code": "provider_retired",
            "checked_at": unix_millis()
        })));
    }
    let (account, credential) = state
        .db
        .upstream_account_with_credential(account_id, state.config.key_pepper.as_bytes())
        .await?;
    if credential.validate(unix_millis()).is_err() {
        return Ok(Json(json!({
            "account_id": account_id,
            "status": "unhealthy",
            "error_code": "credential_invalid",
            "checked_at": unix_millis()
        })));
    }
    let checked_at = unix_millis();
    if let Some((failure, retry_at)) = state
        .db
        .upstream_manual_health_suppression(account_id, account.credential_generation, checked_at)
        .await?
    {
        let error_code = match failure.as_str() {
            "quota_exhausted" => "quota_exhausted",
            "rate_limited" => "rate_limited",
            _ => "upstream_unavailable",
        };
        return Ok(Json(json!({
            "account_id": account_id,
            "status": "unhealthy",
            "error_code": error_code,
            "retry_at": retry_at,
            "source": "routing_state",
            "checked_at": checked_at
        })));
    }
    let base_url = validate_config(&account.config)?;
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
            return Ok(Json(json!({
                "account_id": account_id,
                "status": "unhealthy",
                "error_code": "destination_invalid",
                "checked_at": unix_millis()
            })));
        }
    };
    let probe_url = upstream_health_probe_url(&account.driver, &account.config, &base_url);
    let started = Instant::now();
    let request = outbound
        .get(probe_url)
        .header(header::ACCEPT, "application/json")
        .timeout(Duration::from_secs(5));
    let request = if account.driver == "openai-codex" {
        let account_id = match crate::oauth::managed::codex::account_header_value(&credential) {
            Ok(account_id) => account_id,
            Err(_) => {
                return Ok(Json(json!({
                    "account_id": account_id,
                    "status": "unhealthy",
                    "error_code": "credential_invalid",
                    "checked_at": unix_millis()
                })));
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
            return Ok(Json(json!({
                "account_id": account_id,
                "status": "unhealthy",
                "error_code": "credential_invalid",
                "checked_at": unix_millis()
            })));
        }
    };
    let response = request.send().await;
    let latency_ms = started.elapsed().as_millis().min(i64::MAX as u128) as i64;
    let checked_at = unix_millis();
    match response {
        Ok(response) => {
            // Dropping the response without reading its body makes the probe
            // bounded and prevents provider error text or secrets from being
            // copied into logs or the management response.
            let upstream_status = response.status();
            let error_code = health_probe_error(&account.driver, upstream_status);
            let healthy = error_code.is_none();
            Ok(Json(json!({
                "account_id": account_id,
                "status": if healthy { "healthy" } else { "unhealthy" },
                "error_code": error_code,
                "source": "connection_probe",
                "upstream_status": upstream_status.as_u16(),
                "latency_ms": latency_ms,
                "checked_at": checked_at
            })))
        }
        Err(_) => Ok(Json(json!({
            "account_id": account_id,
            "status": "unhealthy",
            "error_code": "connection_failed",
            "latency_ms": latency_ms,
            "checked_at": checked_at
        }))),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::upstream_health_probe_url;

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
    fn cbcnx_health_uses_the_bounded_openai_model_catalog_endpoint() {
        assert_eq!(
            upstream_health_probe_url(
                crate::provider::CBCNX_PROVIDER_DRIVER,
                &json!({}),
                "https://cbcnx.example.test/v1/",
            ),
            "https://cbcnx.example.test/v1/models",
        );
    }
}
