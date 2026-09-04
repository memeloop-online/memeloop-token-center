use super::super::*;

fn upstream_health_probe_url(driver: &str, config: &Value, base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    match driver {
        "openai-codex" => {
            format!("{base}/models?client_version={}", env!("CARGO_PKG_VERSION"))
        }
        "http-json" => {
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
    let base_url = validate_config(&account.config)?;
    let outbound = match network::client_for_config_url(
        &state.http,
        &base_url,
        &account.config,
        credential.proxy(),
        state.config.allow_oauth_loopback,
    )
    .await
    {
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
            .header(header::USER_AGENT, crate::api::codex_transport::USER_AGENT)
            .header(header::CONNECTION, "Keep-Alive")
            .header("originator", "codex-tui")
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
            let authentication_failed = matches!(
                upstream_status,
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            );
            let healthy = !authentication_failed && !upstream_status.is_server_error();
            Ok(Json(json!({
                "account_id": account_id,
                "status": if healthy { "healthy" } else { "unhealthy" },
                "error_code": if healthy { Value::Null } else if authentication_failed { json!("authentication_failed") } else { json!("upstream_unavailable") },
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
                env!("CARGO_PKG_VERSION")
            )
        );
    }
}
