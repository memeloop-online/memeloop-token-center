//! Explicit management read of proxy settings, never a credential export.
use super::super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct ProxySettingsQuery {
    tenant_external_id: String,
}

#[derive(serde::Serialize)]
struct ProxySettings {
    account_id: Uuid,
    proxy_url: Option<String>,
    proxy_network_scope: Option<OutboundScope>,
    updated_at: i64,
    credential_generation: i64,
    supported: bool,
}

pub(in crate::api) async fn get_upstream_proxy_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Query(query): Query<ProxySettingsQuery>,
) -> Result<impl IntoResponse, AppError> {
    // Same authority as changing private transport proxies. Check both the
    // selected tenant and its account before opening the encrypted credential.
    let service = require_service(&headers, &state, "providers:write").await?;
    require_service_tenant(&service, &query.tenant_external_id)?;
    require_global_service(&service)?;
    state
        .db
        .require_upstream_tenant(account_id, &query.tenant_external_id)
        .await?;
    let (account, credential, _, _) = state
        .db
        .upstream_account_with_current_credential(account_id, state.config.key_pepper.as_bytes())
        .await?;
    // Build a dedicated projection; never serialize the credential or account.
    let proxy = credential.proxy();
    let body = ProxySettings {
        account_id,
        proxy_url: proxy.map(|(url, _)| url.to_owned()),
        proxy_network_scope: proxy.map(|(_, scope)| scope),
        updated_at: account.updated_at,
        credential_generation: account.credential_generation,
        supported: credential.supports_transport_proxy(),
    };
    Ok(([(header::CACHE_CONTROL, "private, no-store")], Json(body)))
}
