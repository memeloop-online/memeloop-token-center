//! Private companion reads never enter the public/revision-global service-data cache.
use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, LazyLock};

use crate::{
    AppState,
    error::AppError,
    gateway_body::memory::{ProxyMemoryBudget, ProxyMemoryReservation},
    http_settings::{ClientOptions, HttpClientSource},
};

const PLUGIN_ID: &str = "mtc-health-intelligence";
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
// Separate from inference admission: private operator reads cannot reserve
// customer request memory. Charge actual chunks and JSON expansion, not an
// 8 MiB preallocation for every read.
static ADVICE_MEMORY: LazyLock<ProxyMemoryBudget> =
    LazyLock::new(|| ProxyMemoryBudget::new(64 * 1024 * 1024));

struct AdviceBody {
    bytes: Vec<u8>,
    _memory: Arc<ProxyMemoryReservation>,
}

impl AsRef<[u8]> for AdviceBody {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AdviceQuery {
    tenant_external_id: Option<String>,
}

pub(super) async fn concurrency_advice(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AdviceQuery>,
) -> Result<Response, AppError> {
    let service = super::require_service(&headers, &state, "plugins:read").await?;
    let tenant = super::management_tenant(&service, query.tenant_external_id)?;
    let state = state.pin_application_plugins().await?;
    if state
        .plugins
        .service_data_endpoint(PLUGIN_ID, "health-intelligence")
        .is_none()
    {
        return Err(AppError::NotFound);
    }
    let response = if !service.allows("providers:read") || !service.allows("requests:read") {
        json!({"state":"permission_required"})
    } else if tenant.is_none() {
        json!({"state":"select_tenant"})
    } else if state.config.plugin_advice_tenant.as_deref() != tenant.as_deref() {
        // Do not disclose which other tenant has an integration configured.
        json!({"state":"not_configured"})
    } else {
        match (
            state.config.plugin_advice_origin.as_deref(),
            state.config.plugin_advice_read_token.as_deref(),
        ) {
            (Some(origin), Some(token)) => {
                match read_advice(&state, origin, token, tenant.as_deref().unwrap()).await {
                    Ok(body) => {
                        return Ok((
                            [
                                (header::CACHE_CONTROL, "private, no-store"),
                                (header::CONTENT_TYPE, "application/json"),
                            ],
                            body,
                        )
                            .into_response());
                    }
                    // Never forward upstream bodies, transport errors or secret-bearing URLs.
                    Err(()) => json!({"state":"unavailable"}),
                }
            }
            _ => json!({"state":"not_configured"}),
        }
    };
    Ok((
        [(header::CACHE_CONTROL, "private, no-store")],
        Json(response),
    )
        .into_response())
}

fn companion_url(origin: &str) -> Result<url::Url, ()> {
    let mut url = url::Url::parse(origin).map_err(|_| ())?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || url.host_str().is_none()
    {
        return Err(());
    }
    url.set_path("/api/health-intelligence/concurrency-advice");
    Ok(url)
}

async fn read_advice(
    state: &AppState,
    origin: &str,
    token: &str,
    tenant: &str,
) -> Result<bytes::Bytes, ()> {
    let url = companion_url(origin)?;
    if !(16..=8192).contains(&token.len())
        || token.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(());
    }
    let client = state
        .http
        .http_client(ClientOptions {
            no_retry: true,
            ..Default::default()
        })
        .map_err(|_| ())?;
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        let memory = ADVICE_MEMORY.reservation();
        let mut response = client
            .get(url)
            .bearer_auth(token)
            .header(header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| ())?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|n| n > MAX_BODY_BYTES as u64)
            || !response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|h| h.to_str().ok())
                .is_some_and(|h| {
                    h.split(';')
                        .next()
                        .is_some_and(|v| v.trim() == "application/json")
                })
        {
            return Err(());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
            if chunk.len() > MAX_BODY_BYTES.saturating_sub(body.len()) {
                return Err(());
            }
            if !memory.try_grow(chunk.len(), 2) {
                return Err(());
            }
            body.extend_from_slice(&chunk);
        }
        if !memory.try_reserve_json(&body) {
            return Err(());
        }
        let data: Value = serde_json::from_slice(&body).map_err(|_| ())?;
        validate_advice(&data, tenant)?;
        drop(data);
        let prefix = b"{\"state\":\"ready\",\"data\":";
        let length = prefix.len() + body.len() + 1;
        if !memory.try_grow(length, 1) {
            return Err(());
        }
        let mut bytes = Vec::with_capacity(length);
        bytes.extend_from_slice(prefix);
        bytes.extend_from_slice(&body);
        bytes.push(b'}');
        // Bytes owns the lease through downstream consumption/cancellation.
        Ok(bytes::Bytes::from_owner(AdviceBody {
            bytes,
            _memory: memory,
        }))
    })
    .await
    .map_err(|_| ())?
}

fn validate_advice(data: &Value, tenant: &str) -> Result<(), ()> {
    if data["contract_version"] != "health_concurrency_advice_v1"
        || data["tenant_external_id"].as_str() != Some(tenant)
        || data["application_mode"] != "manual_only"
        || data["experiment"]["enabled"] != false
        || data["experiment"]["state"] != "disabled"
        || !data["stale"].is_boolean()
        || !data["series"].is_array()
        || !data["limitations"].is_array()
        || !data["uncertainty"].is_object()
    {
        return Err(());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_origin_cannot_redirect_authority_through_configuration() {
        for origin in [
            "http://private.example",
            "https://user:secret@example.com",
            "https://example.com/path",
            "https://example.com/?tenant=other",
            "https://example.com/#fragment",
        ] {
            assert!(companion_url(origin).is_err());
        }
        assert_eq!(
            companion_url("https://example.com").unwrap().path(),
            "/api/health-intelligence/concurrency-advice"
        );
        assert!(companion_url("http://127.0.0.1:3000").is_ok());
    }

    #[test]
    fn private_snapshot_requires_exact_tenant_and_read_only_contract() {
        let valid = json!({
            "contract_version":"health_concurrency_advice_v1",
            "tenant_external_id":"tenant-a","application_mode":"manual_only",
            "experiment":{"enabled":false,"state":"disabled"},
            "stale":true,"series":[],"limitations":[],"uncertainty":{}
        });
        assert!(validate_advice(&valid, "tenant-a").is_ok());
        assert!(validate_advice(&valid, "tenant-b").is_err());
        let mut active = valid.clone();
        active["experiment"]["enabled"] = json!(true);
        assert!(validate_advice(&active, "tenant-a").is_err());
        let mut public = valid;
        public.as_object_mut().unwrap().remove("tenant_external_id");
        assert!(validate_advice(&public, "tenant-a").is_err());
    }
}
