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
    source: String,
    proxy_id: Option<Uuid>,
    group_id: Option<Uuid>,
    selected_member_id: Option<Uuid>,
    configuration_state: &'static str,
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
    let mut policy = state
        .db
        .get_network_proxy_selection(account_id, &query.tenant_external_id)
        .await?;
    state
        .transport_proxy_groups
        .describe_network_proxy_selection(account_id, &mut policy);
    let source = policy["desired"]["mode"]
        .as_str()
        .unwrap_or("credential")
        .to_owned();
    let selected = state.transport_proxy_groups.select_config(
        account_id,
        account.credential_generation,
        &credential,
        &account.config,
    );
    let configuration_state = if selected.is_err() {
        "pending"
    } else if source == "credential" {
        "credential"
    } else {
        "applied"
    };
    let effective = selected
        .as_ref()
        .ok()
        .map(|selection| &selection.credential);
    let proxy = effective.and_then(UpstreamCredential::proxy);
    let identity =
        |value: &serde_json::Value| value.as_str().and_then(|id| Uuid::parse_str(id).ok());
    let body = ProxySettings {
        account_id,
        proxy_url: proxy.map(|(url, _)| url.to_owned()),
        proxy_network_scope: proxy.map(|(_, scope)| scope),
        updated_at: account.updated_at,
        credential_generation: account.credential_generation,
        supported: credential.supports_transport_proxy(),
        source,
        proxy_id: identity(&policy["desired"]["proxy_id"]),
        group_id: identity(&policy["desired"]["group_id"]),
        selected_member_id: selected
            .as_ref()
            .ok()
            .and_then(|selection| selection.egress_snapshot())
            .and_then(|egress| egress.member_id),
        configuration_state,
    };
    Ok(([(header::CACHE_CONTROL, "private, no-store")], Json(body)))
}
