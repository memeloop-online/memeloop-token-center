use super::super::*;
use super::{
    accounts::{
        validate_provider_config_schema, validate_provider_schema, validate_upstream_destination,
    },
    oauth::reauthorization_target,
};
use crate::oauth::{copilot, kimi_device, managed::kimi};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct StartDeviceOAuthRequest {
    #[serde(default = "default_tenant")]
    pub(super) tenant_external_id: String,
    pub(super) account_name: String,
    #[serde(default)]
    upstream_account_id: Option<Uuid>,
    #[serde(default)]
    proxy_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct PollDeviceOAuthRequest {
    pub(super) session_token: String,
}

#[derive(Clone, Copy)]
pub(super) enum DeviceProvider {
    Kimi,
    Copilot,
}

impl DeviceProvider {
    fn drivers(self) -> (&'static str, &'static str) {
        match self {
            Self::Kimi => (kimi::PROVIDER_DRIVER, kimi_device::OAUTH_DRIVER),
            Self::Copilot => (copilot::PROVIDER_DRIVER, copilot::OAUTH_DRIVER),
        }
    }

    pub(super) async fn prepare_start(
        self,
        state: &AppState,
        service: &AuthenticatedService,
        request: &mut StartDeviceOAuthRequest,
    ) -> Result<(Value, Option<OAuthReauthorizationTarget>, Option<String>), AppError> {
        require_service_tenant(service, &request.tenant_external_id)?;
        if request.upstream_account_id.is_some() && request.proxy_url.is_some() {
            return Err(AppError::BadRequest(
                "reauthorization cannot change the transport proxy; use the transport-proxy endpoint"
                    .into(),
            ));
        }
        if let Some(proxy) = request.proxy_url.as_deref() {
            require_global_service(service)?;
            crate::provider::validate_oauth_remote_dns_proxy_url(
                proxy,
                state.config.allow_oauth_loopback,
            )?;
        }

        let (driver, oauth_driver) = self.drivers();
        let config = if let Some(account_id) = request.upstream_account_id {
            state
                .db
                .upstream_account_for_reauthorization(account_id, &request.tenant_external_id)
                .await?
                .config
        } else {
            match self {
                Self::Kimi => kimi::native_import_config(),
                Self::Copilot => json!({"base_url": copilot::BASE_URL, "network_scope": "public"}),
            }
        };
        validate_provider_config_schema(state, driver, &config)?;
        let target = reauthorization_target(
            state,
            request.upstream_account_id,
            &request.tenant_external_id,
            &request.account_name,
            driver,
            &config,
            oauth_driver,
        )
        .await?;
        let proxy_url = if let Some(target) = &target {
            state
                .db
                .upstream_oauth_reauthorization_proxy_snapshot(
                    target.account_id,
                    &request.tenant_external_id,
                    target.expected_updated_at,
                    target.expected_credential_generation,
                    oauth_driver,
                    state.config.key_pepper.as_bytes(),
                )
                .await?
        } else {
            request.proxy_url.take()
        };
        Ok((config, target, proxy_url))
    }

    pub(super) async fn consumed(
        self,
        state: &AppState,
        service: &AuthenticatedService,
        account_id: Uuid,
        tenant_external_id: &str,
    ) -> Result<Response, AppError> {
        require_service_tenant(service, tenant_external_id)?;
        let mut account = state
            .db
            .upstream_account_for_reauthorization(account_id, tenant_external_id)
            .await?;
        if matches!(self, Self::Kimi) {
            super::restrict_transport_proxy_capability(service, &mut account);
        }
        Ok((
            StatusCode::OK,
            Json(super::config_secrets::public_account(state, account)?),
        )
            .into_response())
    }
}

pub(super) fn pending(retry_after_seconds: u64) -> Response {
    (
        StatusCode::ACCEPTED,
        Json(json!({"status":"pending", "retry_after_seconds":retry_after_seconds})),
    )
        .into_response()
}

pub(super) enum DeviceIdentity {
    Kimi,
    Copilot(String),
}

pub(super) struct ReadyDeviceLogin {
    pub(super) session_id: Uuid,
    pub(super) tenant_external_id: String,
    pub(super) account_name: String,
    pub(super) provider_config: Value,
    pub(super) credential: UpstreamCredential,
    pub(super) reauthorize: Option<OAuthReauthorizationTarget>,
    pub(super) identity: DeviceIdentity,
}

pub(super) async fn finish_device_login(
    state: &AppState,
    service: &AuthenticatedService,
    lease_owner: Uuid,
    ready: ReadyDeviceLogin,
) -> Result<Response, AppError> {
    let (provider, refresh_url) = match &ready.identity {
        DeviceIdentity::Kimi => (DeviceProvider::Kimi, kimi::TOKEN_ENDPOINT),
        DeviceIdentity::Copilot(_) => (DeviceProvider::Copilot, copilot::TOKEN_ENDPOINT),
    };
    let (driver, oauth_driver) = provider.drivers();
    require_service_tenant(service, &ready.tenant_external_id)?;
    validate_provider_schema(state, driver, &ready.provider_config, &ready.credential)?;
    validate_upstream_destination(driver, &ready.provider_config, service, state).await?;
    let reauthorizing = ready.reauthorize.is_some();
    let pepper = state.config.key_pepper.as_bytes();
    let mut account = match ready.reauthorize {
        Some(target) => {
            if let DeviceIdentity::Copilot(stable_account_id) = ready.identity {
                let current = state
                    .db
                    .upstream_oauth_identity_credential(target.account_id, pepper)
                    .await?;
                if copilot::copilot_account_id(&current)? != stable_account_id {
                    return Err(AppError::Conflict(
                        "reauthorization must use the same GitHub account".into(),
                    ));
                }
            }
            state
                .db
                .reauthorize_upstream_account(
                    target.account_id,
                    ReauthorizeUpstreamAccountInput {
                        tenant_external_id: ready.tenant_external_id,
                        expected_updated_at: target.expected_updated_at,
                        expected_credential_generation: target.expected_credential_generation,
                        driver: driver.to_owned(),
                        oauth_session_id: ready.session_id,
                        oauth_driver: oauth_driver.to_owned(),
                        oauth_refresh_url: Some(refresh_url.to_owned()),
                        provider_config: Some(ready.provider_config),
                        credential: ready.credential,
                    },
                    pepper,
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
                        driver: driver.to_owned(),
                        config: ready.provider_config,
                        credential: ready.credential,
                        oauth_session_id: Some(ready.session_id),
                        oauth_driver: Some(oauth_driver.to_owned()),
                        oauth_refresh_url: Some(refresh_url.to_owned()),
                    },
                    pepper,
                )
                .await?
        }
    };
    state
        .db
        .finish_oauth_login_session(ready.session_id, lease_owner, account.id, unix_millis())
        .await?;
    match provider {
        DeviceProvider::Kimi => {
            // The device response does not establish a Kimi user identity.
            tracing::info!(account_id = %account.id, operator_service_id = ?service.service_id,
                reauthorizing, identity_verification = "operator_selected_account",
                "Kimi native device authorization completed");
            if account.status == "active" {
                super::trigger_upstream_model_sync(state.clone(), account.id);
            }
            super::restrict_transport_proxy_capability(service, &mut account);
        }
        DeviceProvider::Copilot => super::trigger_upstream_model_sync(state.clone(), account.id),
    }
    let status = if reauthorizing {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((
        status,
        Json(super::config_secrets::public_account(state, account)?),
    )
        .into_response())
}
