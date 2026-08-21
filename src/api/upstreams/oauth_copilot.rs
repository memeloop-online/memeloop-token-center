use super::super::*;
use super::{
    accounts::{
        validate_provider_config_schema, validate_provider_schema, validate_upstream_destination,
    },
    oauth::reauthorization_target,
};
use crate::oauth::copilot;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct StartCopilotOAuthRequest {
    #[serde(default = "default_tenant")]
    tenant_external_id: String,
    account_name: String,
    #[serde(default)]
    upstream_account_id: Option<Uuid>,
}

pub(in crate::api) async fn start_copilot_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<StartCopilotOAuthRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "oauth:write").await?;
    require_service_tenant(&service, &body.tenant_external_id)?;
    let provider_config = if let Some(account_id) = body.upstream_account_id {
        state
            .db
            .upstream_account_for_reauthorization(account_id, &body.tenant_external_id)
            .await?
            .config
    } else {
        json!({"base_url": copilot::BASE_URL, "network_scope": "public"})
    };
    validate_provider_config_schema(&state, copilot::PROVIDER_DRIVER, &provider_config)?;
    let reauthorize = reauthorization_target(
        &state,
        body.upstream_account_id,
        &body.tenant_external_id,
        &body.account_name,
        copilot::PROVIDER_DRIVER,
        &provider_config,
        copilot::OAUTH_DRIVER,
    )
    .await?;
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
                reauthorize,
            },
            state.config.key_pepper.as_bytes(),
            unix_millis(),
            state.config.allow_oauth_loopback,
        )
        .await?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct PollCopilotOAuthRequest {
    session_token: String,
}

pub(in crate::api) async fn poll_copilot_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PollCopilotOAuthRequest>,
) -> Result<Response, AppError> {
    let service = require_service(&headers, &state, "oauth:write").await?;
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
        } => Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "pending",
                "retry_after_seconds": retry_after_seconds
            })),
        )
            .into_response()),
        copilot::CopilotDevicePollResult::Consumed {
            account_id,
            tenant_external_id,
        } => {
            require_service_tenant(&service, &tenant_external_id)?;
            let account = state
                .db
                .upstream_account_for_reauthorization(account_id, &tenant_external_id)
                .await?;
            Ok((StatusCode::OK, Json(account)).into_response())
        }
        copilot::CopilotDevicePollResult::Ready { lease_owner, login } => {
            finish_copilot_login(&state, &service, lease_owner, *login).await
        }
    }
}

async fn finish_copilot_login(
    state: &AppState,
    service: &AuthenticatedService,
    lease_owner: Uuid,
    ready: copilot::ReadyCopilotDeviceLogin,
) -> Result<Response, AppError> {
    require_service_tenant(service, &ready.tenant_external_id)?;
    validate_provider_schema(
        state,
        copilot::PROVIDER_DRIVER,
        &ready.provider_config,
        &ready.credential,
    )?;
    validate_upstream_destination(
        copilot::PROVIDER_DRIVER,
        &ready.provider_config,
        service,
        state,
    )
    .await?;
    let reauthorizing = ready.reauthorize.is_some();
    let account = match ready.reauthorize {
        Some(target) => {
            let (_, current) = state
                .db
                .upstream_account_with_credential(
                    target.account_id,
                    state.config.key_pepper.as_bytes(),
                )
                .await?;
            if copilot::copilot_account_id(&current)? != ready.stable_account_id {
                return Err(AppError::Conflict(
                    "reauthorization must use the same GitHub account".into(),
                ));
            }
            state
                .db
                .reauthorize_upstream_account(
                    target.account_id,
                    ReauthorizeUpstreamAccountInput {
                        tenant_external_id: ready.tenant_external_id,
                        expected_updated_at: target.expected_updated_at,
                        driver: copilot::PROVIDER_DRIVER.to_owned(),
                        oauth_session_id: ready.session_id,
                        oauth_driver: copilot::OAUTH_DRIVER.to_owned(),
                        oauth_refresh_url: Some(copilot::TOKEN_ENDPOINT.to_owned()),
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
                        driver: copilot::PROVIDER_DRIVER.to_owned(),
                        config: ready.provider_config,
                        credential: ready.credential,
                        oauth_session_id: Some(ready.session_id),
                        oauth_driver: Some(copilot::OAUTH_DRIVER.to_owned()),
                        oauth_refresh_url: Some(copilot::TOKEN_ENDPOINT.to_owned()),
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
    super::trigger_upstream_model_sync(state.clone(), account.id);
    Ok((
        if reauthorizing {
            StatusCode::OK
        } else {
            StatusCode::CREATED
        },
        Json(account),
    )
        .into_response())
}
