use super::super::*;
use super::accounts::{
    validate_provider_config_schema, validate_provider_schema, validate_upstream_destination,
};
use crate::oauth::authorization_code::{self, ClientConfig, CompleteResult, StartInput};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct StartRequest {
    #[serde(default = "default_tenant")]
    tenant_external_id: String,
    account_name: String,
    #[serde(default)]
    upstream_account_id: Option<Uuid>,
    provider_driver: String,
    #[serde(default)]
    provider_config: Value,
    #[serde(default)]
    client: Option<ClientConfig>,
    #[serde(default)]
    proxy_url: Option<String>,
    #[serde(default)]
    proxy_network_scope: Option<crate::network::OutboundScope>,
}

pub(in crate::api) async fn start_authorization_code_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut body): Json<StartRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "oauth:write").await?;
    require_service_tenant(&service, &body.tenant_external_id)?;
    let state = state.pin_application_plugins().await?;
    // Supplying a client, callback or explicit proxy changes token destination
    // authority and is restricted to the global operator, just like plugin install.
    if service.tenant_external_id.is_some() {
        return Err(AppError::Forbidden);
    }
    let mut current_credential = None;
    let reauthorize = if let Some(account_id) = body.upstream_account_id {
        if body.proxy_url.is_some() || body.proxy_network_scope.is_some() {
            return Err(AppError::BadRequest(
                "reauthorization cannot change the transport proxy".into(),
            ));
        }
        let (account, credential, _, flow) = state
            .db
            .upstream_account_with_current_credential(
                account_id,
                state.config.key_pepper.as_bytes(),
            )
            .await?;
        if account.tenant_external_id.as_deref() != Some(body.tenant_external_id.as_str()) {
            return Err(AppError::Forbidden);
        }
        if account.driver != body.provider_driver
            || account.name != body.account_name.trim()
            || flow.as_deref() != Some(authorization_code::FLOW)
            || !account.can_reauthorize
            || (!body.provider_config.is_null() && account.config != body.provider_config)
        {
            return Err(AppError::Conflict(
                "reauthorization must use the existing provider, name and configuration".into(),
            ));
        }
        body.account_name = account.name;
        body.provider_config = account.config;
        if let Some((proxy, scope)) = credential.proxy() {
            body.proxy_url = Some(proxy.to_owned());
            body.proxy_network_scope = Some(scope);
        }
        current_credential = Some(credential);
        Some(crate::oauth::OAuthReauthorizationTarget {
            account_id,
            expected_updated_at: account.updated_at,
            expected_credential_generation: account.credential_generation,
        })
    } else {
        None
    };
    if body.proxy_url.is_some() != body.proxy_network_scope.is_some()
        || body
            .proxy_network_scope
            .is_some_and(|scope| scope != crate::network::OutboundScope::Private)
    {
        return Err(AppError::BadRequest(
            "proxy URL must be paired with private network scope".into(),
        ));
    }
    if let Some(proxy) = &body.proxy_url {
        crate::provider::validate_proxy_url(proxy)?;
    }
    if body.provider_driver == crate::provider::antigravity::DRIVER {
        let config = crate::provider::antigravity::Config::from_account(&body.provider_config)?;
        let _ = crate::network::client_for_config_url_no_retry(
            &state.http,
            &config.control_url,
            &body.provider_config,
            body.proxy_url.as_deref().zip(body.proxy_network_scope),
            state.config.allow_oauth_loopback,
        )
        .await?;
    }
    validate_provider_config_schema(&state, &body.provider_driver, &body.provider_config)?;
    validate_upstream_destination(
        &body.provider_driver,
        &body.provider_config,
        &service,
        &state,
    )
    .await?;
    let provider = state
        .providers
        .get(&body.provider_driver)
        .ok_or_else(|| AppError::BadRequest("unknown provider".into()))?;
    let adapter = provider
        .oauth_adapter
        .clone()
        .ok_or_else(|| AppError::BadRequest("provider does not offer OAuth".into()))?;
    let client = match current_credential.as_ref() {
        Some(credential) => authorization_code::reauthorization_client(
            credential,
            body.client,
            &body.provider_driver,
            &adapter.refresh_url,
        )?,
        None => match body.client {
            Some(client) => client,
            None => authorization_code::deployment_client_default(&body.provider_driver)?,
        },
    };
    Ok(Json(
        authorization_code::start(
            &state.db,
            StartInput {
                reauthorize,
                application_plugin_revision: state.application_plugin_revision(),
                tenant_external_id: body.tenant_external_id,
                account_name: body.account_name,
                provider_driver: body.provider_driver,
                provider_config: body.provider_config,
                operator_service_id: service.service_id,
                client,
                adapter,
                proxy_url: body.proxy_url,
                proxy_network_scope: body.proxy_network_scope,
            },
            state.config.key_pepper.as_bytes(),
            unix_millis(),
        )
        .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct CompleteRequest {
    session_token: String,
    callback_url: String,
}

pub(in crate::api) async fn complete_authorization_code_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CompleteRequest>,
) -> Result<Response, AppError> {
    let service = require_service(&headers, &state, "oauth:write").await?;
    let revision = authorization_code::session_application_revision(
        &body.session_token,
        state.config.key_pepper.as_bytes(),
        service.tenant_external_id.as_deref(),
        service.service_id,
        unix_millis(),
    )?;
    let state = state.pin_oauth_application_revision(revision).await?;
    match authorization_code::complete(
        &state.db,
        &state.http,
        &body.session_token,
        &body.callback_url,
        service.tenant_external_id.as_deref(),
        service.service_id,
        state.config.key_pepper.as_bytes(),
        unix_millis(),
        state.config.allow_oauth_loopback,
    )
    .await?
    {
        CompleteResult::Pending {
            retry_after_seconds,
        } => Ok((
            StatusCode::ACCEPTED,
            Json(json!({"status": "pending", "retry_after_seconds": retry_after_seconds})),
        )
            .into_response()),
        CompleteResult::Consumed {
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
        CompleteResult::Ready { lease_owner, login } => {
            let mut ready = *login;
            require_service_tenant(&service, &ready.tenant_external_id)?;
            // Tokens are already durably staged before this recoverable read.
            // Project discovery failure must never discard a newly issued refresh token.
            if ready.reauthorize.is_none()
                && ready.provider_driver == crate::provider::antigravity::DRIVER
            {
                let config =
                    crate::provider::antigravity::Config::from_account(&ready.provider_config)?;
                let native = crate::provider::antigravity::NativeClient {
                    http: &state.http,
                    credential: &ready.credential,
                    config: &config,
                    allow_test_loopback: state.config.allow_oauth_loopback,
                };
                ready.provider_config["project_id"] =
                    Value::String(native.discover_project().await?);
            }
            validate_provider_schema(
                &state,
                &ready.provider_driver,
                &ready.provider_config,
                &ready.credential,
            )?;
            validate_upstream_destination(
                &ready.provider_driver,
                &ready.provider_config,
                &service,
                &state,
            )
            .await?;
            let reauthorizing = ready.reauthorize.is_some();
            let mut account = if let Some(target) = ready.reauthorize {
                state
                    .db
                    .reauthorize_upstream_account(
                        target.account_id,
                        crate::db::ReauthorizeUpstreamAccountInput {
                            tenant_external_id: ready.tenant_external_id,
                            expected_updated_at: target.expected_updated_at,
                            expected_credential_generation: target.expected_credential_generation,
                            driver: ready.provider_driver,
                            oauth_session_id: ready.session_id,
                            oauth_driver: authorization_code::FLOW.into(),
                            oauth_refresh_url: Some(ready.refresh_url),
                            provider_config: None,
                            credential: ready.credential,
                        },
                        state.config.key_pepper.as_bytes(),
                    )
                    .await?
            } else {
                state
                    .db
                    .create_upstream_account(
                        CreateUpstreamAccountInput {
                            tenant_external_id: ready.tenant_external_id,
                            name: ready.account_name,
                            driver: ready.provider_driver,
                            config: ready.provider_config,
                            credential: ready.credential,
                            oauth_session_id: Some(ready.session_id),
                            oauth_driver: Some(authorization_code::FLOW.into()),
                            oauth_refresh_url: Some(ready.refresh_url),
                        },
                        state.config.key_pepper.as_bytes(),
                    )
                    .await?
            };
            state
                .db
                .finish_oauth_login_session(
                    ready.session_id,
                    lease_owner,
                    account.id,
                    unix_millis(),
                )
                .await?;
            if account.status == "active" {
                super::trigger_upstream_model_sync(state.clone(), account.id);
            }
            super::restrict_transport_proxy_capability(&service, &mut account);
            Ok((
                if reauthorizing {
                    StatusCode::OK
                } else {
                    StatusCode::CREATED
                },
                Json(super::config_secrets::public_account(&state, account)?),
            )
                .into_response())
        }
    }
}
