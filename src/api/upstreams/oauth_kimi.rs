use super::super::*;
use super::{
    accounts::validate_upstream_destination,
    oauth_device::{
        DeviceIdentity, DeviceProvider, PollDeviceOAuthRequest, ReadyDeviceLogin,
        StartDeviceOAuthRequest, finish_device_login, pending,
    },
};
use crate::oauth::{kimi_device as device, managed::kimi};

pub(in crate::api) async fn start_kimi_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut body): Json<StartDeviceOAuthRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "oauth:write").await?;
    let state = state.pin_application_plugins().await?;
    let (config, target, proxy_url) = DeviceProvider::Kimi
        .prepare_start(&state, &service, &mut body)
        .await?;
    let (device_id, previous_scope) = if let Some(target) = &target {
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
        (device_id, previous_scope)
    } else {
        (None, None)
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

pub(in crate::api) async fn poll_kimi_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PollDeviceOAuthRequest>,
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
        } => Ok(pending(retry_after_seconds)),
        device::KimiDevicePollResult::Consumed {
            account_id,
            tenant_external_id,
        } => {
            DeviceProvider::Kimi
                .consumed(&state, &service, account_id, &tenant_external_id)
                .await
        }
        device::KimiDevicePollResult::Ready { lease_owner, login } => {
            finish_device_login(
                &state,
                &service,
                lease_owner,
                ReadyDeviceLogin {
                    session_id: login.session_id,
                    tenant_external_id: login.tenant_external_id,
                    account_name: login.account_name,
                    provider_config: login.provider_config,
                    credential: login.credential,
                    reauthorize: login.reauthorize,
                    identity: DeviceIdentity::Kimi,
                },
            )
            .await
        }
    }
}
