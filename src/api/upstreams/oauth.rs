use super::super::*;
use super::accounts::{
    default_provider_driver, validate_provider_config_schema, validate_provider_schema,
    validate_upstream_destination,
};

async fn reauthorization_target(
    state: &AppState,
    account_id: Option<Uuid>,
    tenant_external_id: &str,
    account_name: &str,
    provider_driver: &str,
    provider_config: &Value,
    oauth_driver: &str,
) -> Result<Option<OAuthReauthorizationTarget>, AppError> {
    let Some(account_id) = account_id else {
        return Ok(None);
    };
    let account = state
        .db
        .upstream_account_for_reauthorization(account_id, tenant_external_id)
        .await?;
    if !account.can_reauthorize {
        return Err(AppError::BadRequest(
            "upstream account does not support interactive reauthorization".into(),
        ));
    }
    let existing_oauth_driver = if account.connection_method == "subscription_bridge" {
        "subscription_bridge"
    } else if state
        .providers
        .get(&account.driver)
        .is_some_and(|provider| provider.oauth_adapter.is_some())
    {
        "provider_adapter"
    } else {
        "cursor"
    };
    if account.name != account_name.trim()
        || account.driver != provider_driver
        || account.config != *provider_config
        || existing_oauth_driver != oauth_driver
    {
        return Err(AppError::Conflict(
            "reauthorization must use the existing upstream name, driver, configuration, and OAuth lifecycle".into(),
        ));
    }
    Ok(Some(OAuthReauthorizationTarget {
        account_id,
        expected_updated_at: account.updated_at,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct StartCursorOAuthRequest {
    #[serde(default = "default_tenant")]
    tenant_external_id: String,
    account_name: String,
    #[serde(default = "default_provider_driver")]
    provider_driver: String,
    provider_config: Value,
    #[serde(default)]
    endpoints: Option<CursorOAuthEndpoints>,
    #[serde(default)]
    upstream_account_id: Option<Uuid>,
}

pub(in crate::api) async fn start_cursor_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<StartCursorOAuthRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "oauth:write").await?;
    require_service_tenant(&service, &body.tenant_external_id)?;
    if !state.providers.contains(&body.provider_driver) {
        return Err(AppError::BadRequest(format!(
            "unknown provider driver: {}",
            body.provider_driver
        )));
    }
    validate_provider_config_schema(&state, &body.provider_driver, &body.provider_config)?;
    let reauthorize = reauthorization_target(
        &state,
        body.upstream_account_id,
        &body.tenant_external_id,
        &body.account_name,
        &body.provider_driver,
        &body.provider_config,
        "cursor",
    )
    .await?;
    validate_upstream_destination(
        &body.provider_driver,
        &body.provider_config,
        &service,
        &state,
    )
    .await?;
    let endpoints = body.endpoints.unwrap_or_default();
    if !state.config.allow_oauth_loopback && endpoints != CursorOAuthEndpoints::default() {
        return Err(AppError::BadRequest(
            "custom Cursor OAuth endpoints are disabled".into(),
        ));
    }
    Ok(Json(start_cursor_login(
        StartCursorLogin {
            tenant_external_id: body.tenant_external_id,
            account_name: body.account_name,
            provider_driver: body.provider_driver,
            provider_config: body.provider_config,
            endpoints,
            oauth_driver: "cursor".to_owned(),
            reauthorize,
        },
        state.config.key_pepper.as_bytes(),
        unix_millis(),
    )?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct StartProviderAdapterOAuthRequest {
    #[serde(default = "default_tenant")]
    tenant_external_id: String,
    account_name: String,
    provider_driver: String,
    provider_config: Value,
    #[serde(default)]
    upstream_account_id: Option<Uuid>,
}

pub(in crate::api) async fn start_provider_adapter_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<StartProviderAdapterOAuthRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "oauth:write").await?;
    require_service_tenant(&service, &body.tenant_external_id)?;
    let provider = state.providers.get(&body.provider_driver).ok_or_else(|| {
        AppError::BadRequest(format!("unknown provider driver: {}", body.provider_driver))
    })?;
    let adapter = provider.oauth_adapter.as_ref().ok_or_else(|| {
        AppError::BadRequest(format!(
            "provider {} does not contribute an OAuth adapter",
            body.provider_driver
        ))
    })?;
    if adapter.api_version != "oauth-adapter-v1"
        || adapter.flow_kind != crate::provider::OAuthFlowKind::CursorPkce
    {
        return Err(AppError::BadRequest(format!(
            "provider {} uses an unsupported OAuth adapter contract",
            body.provider_driver
        )));
    }
    validate_provider_config_schema(&state, &body.provider_driver, &body.provider_config)?;
    let reauthorize = reauthorization_target(
        &state,
        body.upstream_account_id,
        &body.tenant_external_id,
        &body.account_name,
        &body.provider_driver,
        &body.provider_config,
        "provider_adapter",
    )
    .await?;
    validate_upstream_destination(
        &body.provider_driver,
        &body.provider_config,
        &service,
        &state,
    )
    .await?;
    Ok(Json(start_cursor_login(
        StartCursorLogin {
            tenant_external_id: body.tenant_external_id,
            account_name: body.account_name,
            provider_driver: body.provider_driver,
            provider_config: body.provider_config,
            endpoints: CursorOAuthEndpoints {
                login_url: adapter.login_url.clone(),
                poll_url: adapter.poll_url.clone(),
                refresh_url: adapter.refresh_url.clone(),
            },
            oauth_driver: "provider_adapter".to_owned(),
            reauthorize,
        },
        state.config.key_pepper.as_bytes(),
        unix_millis(),
    )?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct PollCursorOAuthRequest {
    session_token: String,
}

pub(in crate::api) async fn poll_cursor_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PollCursorOAuthRequest>,
) -> Result<Response, AppError> {
    let service = require_service(&headers, &state, "oauth:write").await?;
    match poll_cursor_login(
        &state.http,
        &body.session_token,
        state.config.key_pepper.as_bytes(),
        unix_millis(),
        service.tenant_external_id.as_deref(),
        state.config.allow_oauth_loopback,
    )
    .await?
    {
        CursorPollResult::Pending {
            retry_after_seconds,
        } => Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "pending",
                "retry_after_seconds": retry_after_seconds
            })),
        )
            .into_response()),
        CursorPollResult::Ready(ready) => {
            let ready = *ready;
            require_service_tenant(&service, &ready.tenant_external_id)?;
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
            let account = match ready.reauthorize {
                Some(target) => {
                    state
                        .db
                        .reauthorize_upstream_account(
                            target.account_id,
                            ReauthorizeUpstreamAccountInput {
                                tenant_external_id: ready.tenant_external_id,
                                expected_updated_at: target.expected_updated_at,
                                driver: ready.provider_driver,
                                oauth_session_id: ready.session_id,
                                oauth_driver: ready.oauth_driver,
                                oauth_refresh_url: Some(ready.refresh_url),
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
                                driver: ready.provider_driver,
                                config: ready.provider_config,
                                credential: ready.credential,
                                oauth_session_id: Some(ready.session_id),
                                oauth_driver: Some(ready.oauth_driver),
                                oauth_refresh_url: Some(ready.refresh_url),
                            },
                            state.config.key_pepper.as_bytes(),
                        )
                        .await?
                }
            };
            let status = if reauthorizing {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            Ok((status, Json(account)).into_response())
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct StartSubscriptionBridgeOAuthRequest {
    #[serde(default = "default_tenant")]
    tenant_external_id: String,
    account_name: String,
    provider: String,
    base_url: String,
    bridge_secret: Option<String>,
    #[serde(default)]
    upstream_account_id: Option<Uuid>,
}

pub(in crate::api) async fn start_subscription_bridge_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<StartSubscriptionBridgeOAuthRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "oauth:write").await?;
    require_service_tenant(&service, &body.tenant_external_id)?;
    // Subscription bridges are deliberately in-cluster. Only a global operator may
    // authorize a new private-network destination for a tenant.
    require_global_service(&service)?;
    let provider_config = json!({
        "base_url": body.base_url,
        "provider": body.provider,
        "network_scope": "private"
    });
    let reauthorize = reauthorization_target(
        &state,
        body.upstream_account_id,
        &body.tenant_external_id,
        &body.account_name,
        "cpa-subscription-bridge",
        &provider_config,
        "subscription_bridge",
    )
    .await?;
    Ok(Json(
        start_subscription_bridge_login(
            &state.http,
            StartSubscriptionBridgeLogin {
                tenant_external_id: body.tenant_external_id,
                account_name: body.account_name,
                provider_config,
                bridge_secret: body.bridge_secret,
                allow_loopback: state.config.allow_oauth_loopback,
                reauthorize,
            },
            state.config.key_pepper.as_bytes(),
            unix_millis(),
        )
        .await?,
    ))
}

pub(in crate::api) async fn poll_subscription_bridge_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PollCursorOAuthRequest>,
) -> Result<Response, AppError> {
    let service = require_service(&headers, &state, "oauth:write").await?;
    match poll_subscription_bridge_login(
        &state.http,
        &body.session_token,
        state.config.key_pepper.as_bytes(),
        unix_millis(),
        service.tenant_external_id.as_deref(),
    )
    .await?
    {
        SubscriptionBridgePollResult::Pending {
            retry_after_seconds,
            message,
        } => Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "pending",
                "retry_after_seconds": retry_after_seconds,
                "message": message
            })),
        )
            .into_response()),
        SubscriptionBridgePollResult::Ready(ready) => {
            let ready = *ready;
            require_service_tenant(&service, &ready.tenant_external_id)?;
            validate_provider_schema(
                &state,
                "cpa-subscription-bridge",
                &ready.provider_config,
                &ready.credential,
            )?;
            validate_upstream_destination(
                "cpa-subscription-bridge",
                &ready.provider_config,
                &service,
                &state,
            )
            .await?;
            let reauthorizing = ready.reauthorize.is_some();
            let account = match ready.reauthorize {
                Some(target) => {
                    state
                        .db
                        .reauthorize_upstream_account(
                            target.account_id,
                            ReauthorizeUpstreamAccountInput {
                                tenant_external_id: ready.tenant_external_id,
                                expected_updated_at: target.expected_updated_at,
                                driver: "cpa-subscription-bridge".to_owned(),
                                oauth_session_id: ready.session_id,
                                oauth_driver: "subscription_bridge".to_owned(),
                                oauth_refresh_url: None,
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
                                driver: "cpa-subscription-bridge".to_owned(),
                                config: ready.provider_config,
                                credential: ready.credential,
                                oauth_session_id: Some(ready.session_id),
                                oauth_driver: Some("subscription_bridge".to_owned()),
                                oauth_refresh_url: None,
                            },
                            state.config.key_pepper.as_bytes(),
                        )
                        .await?
                }
            };
            let status = if reauthorizing {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            Ok((status, Json(account)).into_response())
        }
    }
}

pub(in crate::api) async fn refresh_upstream_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "oauth:write").await?;
    let idempotency_key = headers
        .get("idempotency-key")
        .ok_or_else(|| AppError::BadRequest("Idempotency-Key is required".into()))?
        .to_str()
        .map_err(|_| AppError::BadRequest("Idempotency-Key must be valid ASCII".into()))?;
    if let Some(tenant) = service.tenant_external_id.as_deref() {
        state.db.require_upstream_tenant(account_id, tenant).await?;
    }
    Ok(Json(
        refresh_managed_upstream_oauth(&state, account_id, idempotency_key).await?,
    ))
}

/// Shared by the control endpoint and the proactive worker. It never retries
/// an inference request: refresh is a separate, generation-guarded operation.
pub(crate) async fn refresh_managed_upstream_oauth(
    state: &AppState,
    account_id: Uuid,
    idempotency_key: &str,
) -> Result<crate::provider::UpstreamAccountView, AppError> {
    let (driver, refresh_url) = state.db.upstream_oauth_lifecycle(account_id).await?;
    if let Some(replay) = state
        .db
        .begin_upstream_oauth_refresh(
            account_id,
            idempotency_key,
            state.config.key_pepper.as_bytes(),
        )
        .await?
    {
        return Ok(replay);
    }
    let refreshed: Result<UpstreamCredential, AppError> = async {
        let (_, credential) = state
            .db
            .upstream_account_with_credential(account_id, state.config.key_pepper.as_bytes())
            .await?;
        Ok(match driver.as_str() {
            "cursor" | "provider_adapter" => {
                let refresh_scope = if driver == "provider_adapter" {
                    // Interactive plugin adapters are installed by the cluster
                    // administrator and may intentionally be in-cluster.
                    OutboundScope::Private
                } else {
                    OutboundScope::Public
                };
                let refresh_http = network::client_for_url(
                    &state.http,
                    &refresh_url,
                    refresh_scope,
                    state.config.allow_oauth_loopback,
                )
                .await?;
                refresh_cursor_credential(&refresh_http, &refresh_url, &credential, unix_millis())
                    .await?
            }
            _ => {
                let adapter =
                    resolve_managed_oauth_refresh_adapter(&state.providers, &driver, &refresh_url)?;
                refresh_managed_oauth_credential(
                    &state.http,
                    &adapter,
                    &credential,
                    state.config.allow_oauth_loopback,
                )
                .await?
            }
        })
    }
    .await;
    let refreshed = match refreshed {
        Ok(refreshed) => refreshed,
        Err(error) => {
            if let Err(cleanup_error) = state
                .db
                .abort_upstream_oauth_refresh(account_id, idempotency_key)
                .await
            {
                tracing::warn!(%cleanup_error, %account_id, "failed to release OAuth refresh lease");
            }
            return Err(error);
        }
    };
    // A successful authorization-server refresh may have invalidated the old
    // refresh token. Finalize retries therefore stay local; on exhaustion the
    // encrypted pending result and lease remain for exact same-key recovery.
    state
        .db
        .finish_upstream_oauth_refresh(
            account_id,
            refreshed,
            idempotency_key,
            state.config.key_pepper.as_bytes(),
        )
        .await
}
