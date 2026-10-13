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
    #[serde(default)]
    transport_proxy_group: Option<crate::db::transport_proxy_management::OAuthGroupReference>,
}

pub(in crate::api) async fn start_authorization_code_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut body): Json<StartRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "oauth:write").await?;
    require_service_tenant(&service, &body.tenant_external_id)?;
    let state = state.pin_application_plugins().await?;
    body.provider_driver =
        crate::provider::canonicalize_provider_driver(&body.provider_driver).to_owned();
    // Supplying a client, callback or explicit proxy changes token destination
    // authority and is restricted to the global operator, just like plugin install.
    if service.tenant_external_id.is_some() {
        return Err(AppError::Forbidden);
    }
    let mut current_credential = None;
    let mut transport_group = None;
    let reauthorize = if let Some(account_id) = body.upstream_account_id {
        if body.proxy_url.is_some()
            || body.proxy_network_scope.is_some()
            || body.transport_proxy_group.is_some()
        {
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
            || !authorization_code::supports_reauthorization(&credential)
            || (!body.provider_config.is_null() && account.config != body.provider_config)
        {
            return Err(AppError::Conflict(
                "reauthorization must use the existing provider, name and configuration".into(),
            ));
        }
        let selected = state.transport_proxy_groups.select_config(
            account_id,
            account.credential_generation,
            &credential,
            &account.config,
        )?;
        if let Some(encoded) = account
            .config
            .get(crate::db::transport_proxy_management::CONFIG_KEY)
        {
            let stamp: crate::db::transport_proxy_management::BindingStamp =
                serde_json::from_value(encoded.clone()).map_err(|_| {
                    AppError::Conflict("OAuth transport group is unavailable".into())
                })?;
            if let (Some(group_id), Some(group_version)) = (stamp.group_id, stamp.group_version) {
                let binding = state
                    .db
                    .get_transport_binding(account_id, &body.tenant_external_id)
                    .await?;
                let initial_member_id =
                    serde_json::from_value(binding["initial_member_id"].clone()).map_err(|_| {
                        AppError::Conflict("OAuth transport group is unavailable".into())
                    })?;
                transport_group = Some(
                    state
                        .db
                        .snapshot_oauth_transport_group_selected(
                            crate::db::transport_proxy_management::OAuthGroupReference {
                                group_id,
                                expected_group_version: group_version,
                                initial_member_id,
                            },
                            &body.tenant_external_id,
                            Some((account_id, stamp)),
                            selected.credential.proxy().map(|(proxy, _)| proxy),
                            state.config.key_pepper.as_bytes(),
                        )
                        .await?,
                );
            }
        }
        body.account_name = account.name;
        body.provider_config = account.config;
        if let Some((proxy, scope)) = selected.credential.proxy() {
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
    if let Some(reference) = body.transport_proxy_group {
        if !service.allows("providers:write") {
            return Err(AppError::Forbidden);
        }
        if body.proxy_url.is_some() || body.proxy_network_scope.is_some() {
            return Err(AppError::BadRequest(
                "transport group cannot be combined with an individual proxy".into(),
            ));
        }
        let snapshot = state
            .db
            .snapshot_oauth_transport_group(
                reference,
                &body.tenant_external_id,
                None,
                state.config.key_pepper.as_bytes(),
            )
            .await?;
        body.proxy_url = Some(snapshot.proxy_url.clone());
        body.proxy_network_scope = Some(crate::network::OutboundScope::Private);
        transport_group = Some(snapshot);
    }
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
    let (_, secret_patch) =
        super::config_secrets::split_for_storage(&provider.config_schema, &body.provider_config)?;
    crate::provider::validate_provider_adapter_secret_patch(&secret_patch)?;
    let client = match current_credential.as_ref() {
        Some(credential) => {
            if body.provider_driver != crate::provider::antigravity::DRIVER
                && adapter.identity.is_none()
            {
                return Err(AppError::Conflict(
                    "reauthorization requires a declared account identity".into(),
                ));
            }
            authorization_code::validate_installed_contribution(credential, &adapter)?;
            authorization_code::reauthorization_client(
                credential,
                body.client,
                &body.provider_driver,
                &adapter.refresh_url,
            )?
        }
        None => match body.client {
            Some(client) => client,
            None => authorization_code::default_client(&body.provider_driver, &adapter)?,
        },
    };
    Ok(Json(
        authorization_code::start(
            &state.db,
            StartInput {
                transport_group,
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
            let (mut account, credential, _, _) = state
                .db
                .upstream_account_with_current_credential(
                    account_id,
                    state.config.key_pepper.as_bytes(),
                )
                .await?;
            if account.tenant_external_id.as_deref() != Some(tenant_external_id.as_str()) {
                return Err(AppError::Forbidden);
            }
            account.attach_proxy_metadata(&credential, state.config.key_pepper.as_bytes())?;
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
            if let Some(snapshot) = &ready.transport_group {
                state
                    .db
                    .validate_oauth_transport_group(snapshot, state.config.key_pepper.as_bytes())
                    .await?;
            }
            let declared_identity = state
                .providers
                .get(&ready.provider_driver)
                .and_then(|provider| provider.oauth_adapter.as_ref())
                .and_then(|adapter| adapter.identity.clone());
            let identity_supported = ready.provider_driver == crate::provider::antigravity::DRIVER
                || declared_identity.is_some();
            if ready.provider_driver == crate::provider::antigravity::DRIVER {
                // The token is durably staged already. A transient identity read
                // can be retried without exchanging the authorization code again.
                authorization_code::bind_google_identity(&state.http, &mut ready.credential)
                    .await?;
            } else if let Some(identity) = declared_identity.clone() {
                // Same durability contract: the token is staged; a transient
                // identity read failure is retryable without re-exchange.
                authorization_code::bind_declared_identity(
                    &state.http,
                    &mut ready.credential,
                    &identity,
                    &ready.provider_config,
                )
                .await?;
            }
            if let Some(target) = &ready.reauthorize {
                let (account, current, _, _) = state
                    .db
                    .upstream_account_with_current_credential(
                        target.account_id,
                        state.config.key_pepper.as_bytes(),
                    )
                    .await?;
                if account.tenant_external_id.as_deref() != Some(ready.tenant_external_id.as_str())
                    || !identity_supported
                    || !authorization_code::same_reauthorization_identity(
                        &current,
                        &ready.credential,
                    )
                {
                    state
                        .db
                        .reject_generic_oauth_ready(ready.session_id, lease_owner, unix_millis())
                        .await?;
                    return Ok(identity_mismatch_response());
                }
            }
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
            let provider = state
                .providers
                .get(&ready.provider_driver)
                .ok_or_else(|| AppError::BadRequest("unknown provider".into()))?;
            let (public_config, secret_patch) = super::config_secrets::split_for_storage(
                &provider.config_schema,
                &ready.provider_config,
            )?;
            ready.credential = ready
                .credential
                .with_provider_adapter_secret_patch(&secret_patch)?;
            ready.provider_config = public_config;
            let reauthorizing = ready.reauthorize.is_some();
            if reauthorizing && let Some(snapshot) = &ready.transport_group {
                ready.credential = ready
                    .credential
                    .with_transport_proxy(snapshot.proxy_url.clone())?;
            }
            let mut account = if let Some(target) = ready.reauthorize {
                state
                    .db
                    .reauthorize_upstream_account_with_transport(
                        target.account_id,
                        crate::db::ReauthorizeUpstreamAccountInput {
                            tenant_external_id: ready.tenant_external_id,
                            expected_updated_at: target.expected_updated_at,
                            expected_credential_generation: target.expected_credential_generation,
                            driver: ready.provider_driver,
                            oauth_session_id: ready.session_id,
                            oauth_driver: authorization_code::FLOW.into(),
                            oauth_refresh_url: Some(ready.refresh_url),
                            provider_config: Some(ready.provider_config),
                            credential: ready.credential,
                        },
                        ready.transport_group.as_ref(),
                        state.config.key_pepper.as_bytes(),
                    )
                    .await?
            } else {
                state
                    .db
                    .create_upstream_account_with_transport(
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
                        ready.transport_group.as_ref(),
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

fn identity_mismatch_response() -> Response {
    (StatusCode::CONFLICT, Json(json!({"error": {
        "code": "oauth_identity_mismatch",
        "message": "authorization belongs to a different upstream account; the existing account was not changed"
    }}))).into_response()
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn terminated_identity_session_has_a_distinct_machine_code() {
        let response = super::identity_mismatch_response();
        assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], "oauth_identity_mismatch");
    }
}
