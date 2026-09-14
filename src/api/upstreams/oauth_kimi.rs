use super::super::*;
use super::{
    accounts::{
        validate_provider_config_schema, validate_provider_schema, validate_upstream_destination,
    },
    oauth::reauthorization_target,
};
use crate::oauth::{kimi_device as device, managed::kimi};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct StartKimiOAuthRequest {
    #[serde(default = "default_tenant")]
    tenant_external_id: String,
    account_name: String,
    #[serde(default)]
    upstream_account_id: Option<Uuid>,
    #[serde(default)]
    proxy_url: Option<String>,
}

pub(in crate::api) async fn start_kimi_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<StartKimiOAuthRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "oauth:write").await?;
    let state = state.pin_application_plugins().await?;
    require_service_tenant(&service, &body.tenant_external_id)?;
    if body.upstream_account_id.is_some() && body.proxy_url.is_some() {
        return Err(AppError::BadRequest(
            "reauthorization cannot change the transport proxy; use the transport-proxy endpoint"
                .into(),
        ));
    }
    if let Some(proxy) = body.proxy_url.as_deref() {
        require_global_service(&service)?;
        crate::provider::validate_oauth_remote_dns_proxy_url(
            proxy,
            state.config.allow_oauth_loopback,
        )?;
    }
    let config = if let Some(id) = body.upstream_account_id {
        state
            .db
            .upstream_account_for_reauthorization(id, &body.tenant_external_id)
            .await?
            .config
    } else {
        kimi::native_import_config()
    };
    validate_provider_config_schema(&state, kimi::PROVIDER_DRIVER, &config)?;
    let target = reauthorization_target(
        &state,
        body.upstream_account_id,
        &body.tenant_external_id,
        &body.account_name,
        kimi::PROVIDER_DRIVER,
        &config,
        device::OAUTH_DRIVER,
    )
    .await?;
    let (proxy_url, device_id, previous_scope) = if let Some(target) = &target {
        let proxy = state
            .db
            .upstream_oauth_reauthorization_proxy_snapshot(
                target.account_id,
                &body.tenant_external_id,
                target.expected_updated_at,
                target.expected_credential_generation,
                device::OAUTH_DRIVER,
                state.config.key_pepper.as_bytes(),
            )
            .await?;
        let current = state
            .db
            .upstream_oauth_identity_credential(
                target.account_id,
                state.config.key_pepper.as_bytes(),
            )
            .await?;
        let device_id = current
            .adapter_state()
            .and_then(|value| value["device_id"].as_str())
            .map(str::to_owned);
        let previous_scope = current
            .adapter_state()
            .and_then(|value| value["scope"].as_str())
            .map(str::to_owned);
        (proxy, device_id, previous_scope)
    } else {
        (body.proxy_url, None, None)
    };
    validate_upstream_destination(kimi::PROVIDER_DRIVER, &config, &service, &state).await?;
    Ok(Json(
        device::start_kimi_device_login(
            &state.db,
            &state.http,
            device::StartKimiDeviceLogin {
                tenant_external_id: body.tenant_external_id,
                account_name: body.account_name,
                operator_service_id: service.service_id,
                provider_config: config,
                proxy_url,
                device_id,
                previous_scope,
                reauthorize: target,
            },
            state.config.key_pepper.as_bytes(),
            unix_millis(),
            state.config.allow_oauth_loopback,
        )
        .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct PollKimiOAuthRequest {
    session_token: String,
}

pub(in crate::api) async fn poll_kimi_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PollKimiOAuthRequest>,
) -> Result<Response, AppError> {
    let service = require_service(&headers, &state, "oauth:write").await?;
    let state = state.pin_application_plugins().await?;
    match device::poll_kimi_device_login(
        &state.db,
        &state.http,
        &body.session_token,
        state.config.key_pepper.as_bytes(),
        unix_millis(),
        device::KimiDevicePollScope {
            required_tenant: service.tenant_external_id.as_deref(),
            operator_service_id: service.service_id,
        },
        state.config.allow_oauth_loopback,
    )
    .await?
    {
        device::KimiDevicePollResult::Pending {
            retry_after_seconds,
        } => Ok((
            StatusCode::ACCEPTED,
            Json(json!({"status":"pending", "retry_after_seconds":retry_after_seconds})),
        )
            .into_response()),
        device::KimiDevicePollResult::Consumed {
            account_id,
            tenant_external_id,
        } => {
            require_service_tenant(&service, &tenant_external_id)?;
            let mut account = state
                .db
                .upstream_account_for_reauthorization(account_id, &tenant_external_id)
                .await?;
            super::restrict_transport_proxy_capability(&service, &mut account);
            Ok((
                StatusCode::OK,
                Json(super::config_secrets::public_account(&state, account)?),
            )
                .into_response())
        }
        device::KimiDevicePollResult::Ready { lease_owner, login } => {
            finish_kimi_login(&state, &service, lease_owner, *login).await
        }
    }
}

async fn finish_kimi_login(
    state: &AppState,
    service: &AuthenticatedService,
    lease_owner: Uuid,
    ready: device::ReadyKimiDeviceLogin,
) -> Result<Response, AppError> {
    require_service_tenant(service, &ready.tenant_external_id)?;
    validate_provider_schema(
        state,
        kimi::PROVIDER_DRIVER,
        &ready.provider_config,
        &ready.credential,
    )?;
    validate_upstream_destination(
        kimi::PROVIDER_DRIVER,
        &ready.provider_config,
        service,
        state,
    )
    .await?;
    let reauthorizing = ready.reauthorize.is_some();
    let mut account = match ready.reauthorize {
        Some(target) => {
            state
                .db
                .reauthorize_upstream_account(
                    target.account_id,
                    ReauthorizeUpstreamAccountInput {
                        tenant_external_id: ready.tenant_external_id,
                        expected_updated_at: target.expected_updated_at,
                        expected_credential_generation: target.expected_credential_generation,
                        driver: kimi::PROVIDER_DRIVER.to_owned(),
                        oauth_session_id: ready.session_id,
                        oauth_driver: device::OAUTH_DRIVER.to_owned(),
                        oauth_refresh_url: Some(kimi::TOKEN_ENDPOINT.to_owned()),
                        provider_config: Some(ready.provider_config),
                        credential: ready.credential,
                    },
                    state.config.key_pepper.as_bytes(),
                )
                .await?
        }
        None => {
            state
                .db
                .create_upstream_account(
                    CreateUpstreamAccountInput {
                        tenant_external_id: ready.tenant_external_id,
                        name: ready.account_name,
                        driver: kimi::PROVIDER_DRIVER.to_owned(),
                        config: ready.provider_config,
                        credential: ready.credential,
                        oauth_session_id: Some(ready.session_id),
                        oauth_driver: Some(device::OAUTH_DRIVER.to_owned()),
                        oauth_refresh_url: Some(kimi::TOKEN_ENDPOINT.to_owned()),
                    },
                    state.config.key_pepper.as_bytes(),
                )
                .await?
        }
    };
    state
        .db
        .finish_oauth_login_session(ready.session_id, lease_owner, account.id, unix_millis())
        .await?;
    // No documented Kimi user-info identity is present in the device response.
    // Preserve the administrator-selected MTC identity; never claim that a
    // device identifier proves a subscriber identity or silently rename it.
    tracing::info!(account_id = %account.id, operator_service_id = ?service.service_id,
        reauthorizing, identity_verification = "operator_selected_account",
        "Kimi native device authorization completed");
    if account.status == "active" {
        super::trigger_upstream_model_sync(state.clone(), account.id);
    }
    super::restrict_transport_proxy_capability(service, &mut account);
    Ok((
        if reauthorizing {
            StatusCode::OK
        } else {
            StatusCode::CREATED
        },
        Json(super::config_secrets::public_account(state, account)?),
    )
        .into_response())
}
