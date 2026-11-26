use super::super::*;
use super::{
    accounts::validate_upstream_destination,
    oauth_device::{
        DeviceIdentity, DeviceProvider, PollDeviceOAuthRequest, ReadyDeviceLogin,
        StartDeviceOAuthRequest, finish_device_login, pending,
    },
};
use crate::oauth::copilot;

pub(crate) fn trigger_copilot_remint_on_auth_failure(
    state: &AppState,
    route_driver: Option<&str>,
    upstream_account_id: Option<Uuid>,
    request_id: Uuid,
    status: StatusCode,
) {
    if route_driver != Some(copilot::PROVIDER_DRIVER)
        || !matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
    {
        return;
    }
    let Some(account_id) = upstream_account_id else {
        return;
    };
    let refresh_state = state.clone();
    let idempotency_key = format!("copilot-capi-{request_id}");
    tokio::spawn(async move {
        if let Err(error) = super::oauth::refresh_managed_upstream_oauth(
            &refresh_state,
            account_id,
            &idempotency_key,
        )
        .await
        {
            tracing::warn!(
                %account_id,
                status = %status,
                error = %error,
                "Copilot short-token remint failed"
            );
        }
    });
}

pub(in crate::api) async fn start_copilot_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut body): Json<StartDeviceOAuthRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "oauth:write").await?;
    let state = state.pin_application_plugins().await?;
    let (provider_config, reauthorize, session_proxy_url) = DeviceProvider::Copilot
        .prepare_start(&state, &service, &mut body)
        .await?;
    if let Some(proxy_url) = session_proxy_url.as_deref() {
        crate::provider::validate_oauth_remote_dns_proxy_url(
            proxy_url,
            state.config.allow_oauth_loopback,
        )?;
    }
    validate_upstream_destination(copilot::PROVIDER_DRIVER, &provider_config, &service, &state)
        .await?;
    Ok(Json(
        copilot::start_copilot_device_login(
            &state.db,
            &state.http,
            copilot::StartCopilotDeviceLogin {
                tenant_external_id: body.tenant_external_id,
                account_name: body.account_name,
                operator_service_id: service.service_id,
                provider_config,
                proxy_url: session_proxy_url,
                reauthorize,
            },
            state.config.key_pepper.as_bytes(),
            unix_millis(),
            state.config.allow_oauth_loopback,
        )
        .await?,
    ))
}

pub(in crate::api) async fn poll_copilot_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PollDeviceOAuthRequest>,
) -> Result<Response, AppError> {
    let service = require_service(&headers, &state, "oauth:write").await?;
    let state = state.pin_application_plugins().await?;
    match copilot::poll_copilot_device_login(
        &state.db,
        &state.http,
        &body.session_token,
        state.config.key_pepper.as_bytes(),
        unix_millis(),
        copilot::CopilotDevicePollScope {
            required_tenant: service.tenant_external_id.as_deref(),
            operator_service_id: service.service_id,
        },
        state.config.allow_oauth_loopback,
    )
    .await?
    {
        copilot::CopilotDevicePollResult::Pending {
            retry_after_seconds,
        } => Ok(pending(retry_after_seconds)),
        copilot::CopilotDevicePollResult::Consumed {
            account_id,
            tenant_external_id,
        } => {
            DeviceProvider::Copilot
                .consumed(&state, &service, account_id, &tenant_external_id)
                .await
        }
        copilot::CopilotDevicePollResult::Ready { lease_owner, login } => {
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
                    identity: DeviceIdentity::Copilot(login.stable_account_id),
                },
            )
            .await
        }
    }
}
