use super::super::*;
use super::{
    accounts::{
        validate_provider_config_schema, validate_provider_schema,
        validate_upstream_destination_with_proxy,
    },
    oauth::reauthorization_target,
};
use crate::oauth::claude;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct StartClaudeOAuthRequest {
    #[serde(default = "default_tenant")]
    tenant_external_id: String,
    account_name: String,
    #[serde(default)]
    upstream_account_id: Option<Uuid>,
    /// Transport proxy override. When reauthorizing, an absent field inherits
    /// the existing credential proxy, an explicit null clears it, and a value
    /// replaces it. For a new login the field simply selects the proxy.
    #[serde(default, deserialize_with = "deserialize_present_field")]
    proxy_url: Option<Option<String>>,
}

/// Distinguishes an absent request field (`None`) from an explicit JSON null
/// (`Some(None)`); plain `Option<Option<T>>` deserialization collapses both.
fn deserialize_present_field<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<String>::deserialize(deserializer)?))
}

pub(in crate::api) async fn start_claude_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<StartClaudeOAuthRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "oauth:write").await?;
    let state = state.pin_application_plugins().await?;
    require_service_tenant(&service, &body.tenant_external_id)?;
    if let Some(proxy_url) = body.proxy_url.as_ref().and_then(|field| field.as_deref()) {
        require_global_service(&service)?;
        crate::provider::validate_oauth_remote_dns_proxy_url(
            proxy_url,
            state.config.allow_oauth_loopback,
        )?;
    }
    let provider_config = if let Some(account_id) = body.upstream_account_id {
        state
            .db
            .upstream_account_for_reauthorization(account_id, &body.tenant_external_id)
            .await?
            .config
    } else {
        json!({"base_url": "https://api.anthropic.com", "network_scope": "public"})
    };
    validate_provider_config_schema(&state, claude::PROVIDER_DRIVER, &provider_config)?;
    let reauthorize = reauthorization_target(
        &state,
        body.upstream_account_id,
        &body.tenant_external_id,
        &body.account_name,
        claude::PROVIDER_DRIVER,
        &provider_config,
        claude::OAUTH_DRIVER,
    )
    .await?;
    let session_proxy_url = match (reauthorize.as_ref(), body.proxy_url) {
        // Field absent: inherit the existing credential proxy (unchanged).
        (Some(target), None) => {
            state
                .db
                .upstream_oauth_reauthorization_proxy_snapshot(
                    target.account_id,
                    &body.tenant_external_id,
                    target.expected_updated_at,
                    target.expected_credential_generation,
                    claude::OAUTH_DRIVER,
                    state.config.key_pepper.as_bytes(),
                )
                .await?
        }
        // Field present: the request decides. Setting a new proxy was
        // validated above; clearing a fenced proxy is likewise a global
        // operator decision.
        (Some(_), Some(requested)) => {
            require_global_service(&service)?;
            requested
        }
        (None, requested) => requested.flatten(),
    };
    validate_upstream_destination_with_proxy(
        claude::PROVIDER_DRIVER,
        &provider_config,
        session_proxy_url
            .as_deref()
            .map(|url| (url, OutboundScope::Private)),
        &service,
        &state,
    )
    .await?;
    Ok(Json(
        claude::start_claude_login(
            &state.db,
            claude::StartClaudeLogin {
                tenant_external_id: body.tenant_external_id,
                account_name: body.account_name,
                operator_service_id: service.service_id,
                provider_config,
                proxy_url: session_proxy_url,
                reauthorize,
            },
            state.config.key_pepper.as_bytes(),
            unix_millis(),
        )
        .await?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct CompleteClaudeOAuthRequest {
    session_token: String,
    authorization_code: String,
}

pub(in crate::api) async fn complete_claude_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CompleteClaudeOAuthRequest>,
) -> Result<Response, AppError> {
    let service = require_service(&headers, &state, "oauth:write").await?;
    let state = state.pin_application_plugins().await?;
    match claude::complete_claude_login(
        &state.db,
        &state.http,
        &body.session_token,
        &body.authorization_code,
        state.config.key_pepper.as_bytes(),
        unix_millis(),
        claude::ClaudeCompleteScope {
            required_tenant: service.tenant_external_id.as_deref(),
            operator_service_id: service.service_id,
        },
        state.config.allow_oauth_loopback,
    )
    .await?
    {
        claude::ClaudeCompleteResult::Pending {
            retry_after_seconds,
        } => Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "pending",
                "retry_after_seconds": retry_after_seconds
            })),
        )
            .into_response()),
        claude::ClaudeCompleteResult::Consumed {
            account_id,
            tenant_external_id,
        } => {
            require_service_tenant(&service, &tenant_external_id)?;
            let account = state
                .db
                .upstream_account_for_reauthorization(account_id, &tenant_external_id)
                .await?;
            Ok((
                StatusCode::OK,
                Json(super::config_secrets::public_account(&state, account)?),
            )
                .into_response())
        }
        claude::ClaudeCompleteResult::Ready { lease_owner, login } => {
            finish_claude_login(&state, &service, lease_owner, *login).await
        }
    }
}

async fn finish_claude_login(
    state: &AppState,
    service: &AuthenticatedService,
    lease_owner: Uuid,
    ready: claude::ReadyClaudeLogin,
) -> Result<Response, AppError> {
    require_service_tenant(service, &ready.tenant_external_id)?;
    validate_provider_schema(
        state,
        claude::PROVIDER_DRIVER,
        &ready.provider_config,
        &ready.credential,
    )?;
    validate_upstream_destination_with_proxy(
        claude::PROVIDER_DRIVER,
        &ready.provider_config,
        ready.credential.proxy(),
        service,
        state,
    )
    .await?;
    let reauthorizing = ready.reauthorize.is_some();
    let account = match ready.reauthorize {
        Some(target) => {
            let current = state
                .db
                .upstream_oauth_identity_credential(
                    target.account_id,
                    state.config.key_pepper.as_bytes(),
                )
                .await?;
            if claude::claude_account_id(&current)? != claude::claude_account_id(&ready.credential)?
            {
                return Err(AppError::Conflict(
                    "reauthorization must use the same Anthropic account".into(),
                ));
            }
            state
                .db
                .reauthorize_upstream_account(
                    target.account_id,
                    ReauthorizeUpstreamAccountInput {
                        tenant_external_id: ready.tenant_external_id,
                        expected_updated_at: target.expected_updated_at,
                        expected_credential_generation: target.expected_credential_generation,
                        driver: claude::PROVIDER_DRIVER.to_owned(),
                        oauth_session_id: ready.session_id,
                        oauth_driver: ready.oauth_driver,
                        oauth_refresh_url: Some(ready.refresh_url),
                        provider_config: None,
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
                        driver: claude::PROVIDER_DRIVER.to_owned(),
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
        Json(super::config_secrets::public_account(state, account)?),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        db::{
            CreateServiceTokenInput, CreateUpstreamAccountInput, OAuthLoginClaim,
            OAuthLoginSessionReference,
        },
        provider::UpstreamCredential,
    };
    use serde_json::{Value, json};

    const TENANT: &str = "claude-reauth-tenant";
    const ACCOUNT_NAME: &str = "Claude primary";
    const EXISTING_PROXY: &str = "socks5h://proxy-user:proxy-secret@100.64.0.16:1080";
    const REPLACEMENT_PROXY: &str = "socks5h://proxy-user:proxy-secret@100.64.0.17:1080";

    async fn world() -> (tempfile::TempDir, AppState, Uuid) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("claude-reauth.db").display()
        );
        let state = AppState::initialize(Config::for_test(database_url))
            .await
            .expect("app state");
        let account = state
            .db
            .create_upstream_account(
                CreateUpstreamAccountInput {
                    tenant_external_id: TENANT.into(),
                    name: ACCOUNT_NAME.into(),
                    driver: claude::PROVIDER_DRIVER.into(),
                    config: json!({
                        "base_url": "https://api.anthropic.com",
                        "network_scope": "public"
                    }),
                    credential: UpstreamCredential::OAuth {
                        access_token: "claude-access-secret".into(),
                        refresh_token: Some("claude-refresh-secret".into()),
                        expires_at: Some(unix_millis() + 3_600_000),
                        header: "authorization".into(),
                        prefix: "Bearer ".into(),
                        adapter_state: Some(json!({
                            "schema": "anthropic-claude-oauth-v1",
                            "account_id": "719c8604-7a46-4e7d-8fd7-bf6a1be077b5"
                        })),
                        proxy_url: Some(EXISTING_PROXY.into()),
                        proxy_network_scope: Some(OutboundScope::Private),
                    },
                    oauth_session_id: Some(Uuid::now_v7()),
                    oauth_driver: Some(claude::OAUTH_DRIVER.into()),
                    oauth_refresh_url: Some(claude::TOKEN_ENDPOINT.into()),
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .expect("Claude OAuth upstream");
        (directory, state, account.id)
    }

    async fn service_token(state: &AppState, tenant: Option<&str>) -> String {
        state
            .db
            .create_service_token(
                CreateServiceTokenInput {
                    name: format!("claude-reauth-{}", Uuid::now_v7()),
                    scopes: vec!["oauth:write".into()],
                    tenant_external_id: tenant.map(str::to_owned),
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .expect("service token")
            .token
    }

    async fn start(
        state: &AppState,
        token: &str,
        body: Value,
    ) -> Result<axum::response::Response, AppError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {token}")
                .parse()
                .expect("authorization header"),
        );
        let body: StartClaudeOAuthRequest =
            serde_json::from_value(body).expect("start request JSON");
        Ok(
            start_claude_oauth(State(state.clone()), headers, Json(body))
                .await?
                .into_response(),
        )
    }

    /// Opens the started login session the way the completion poll does and
    /// returns the transport proxy recorded for the OAuth exchange.
    async fn started_session_proxy(
        state: &AppState,
        response: axum::response::Response,
    ) -> Option<String> {
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("start response body");
        let start: Value = serde_json::from_slice(&bytes).expect("start response JSON");
        let pepper = state.config.key_pepper.as_bytes();
        let token: Value = crate::provider::open_private_json(
            start["session_token"].as_str().expect("session token"),
            pepper,
            claude::SESSION_AAD,
        )
        .expect("session token opens");
        let reference = OAuthLoginSessionReference {
            session_id: Uuid::parse_str(token["session_id"].as_str().expect("session id"))
                .expect("session id parses"),
            flow_kind: token["flow_kind"].as_str().expect("flow kind").to_owned(),
            tenant_external_id: token["tenant_external_id"]
                .as_str()
                .expect("tenant")
                .to_owned(),
            operator_service_id: token["operator_service_id"]
                .as_str()
                .map(|id| Uuid::parse_str(id).expect("operator id parses")),
            expires_at: token["expires_at"].as_i64().expect("expiry"),
        };
        let claim = state
            .db
            .claim_oauth_login_poll(&reference, unix_millis(), 1)
            .await
            .expect("claim login session");
        let OAuthLoginClaim::Claimed {
            state_ciphertext, ..
        } = claim
        else {
            panic!("pending login session must be claimable");
        };
        let session: Value =
            crate::provider::open_private_json(&state_ciphertext, pepper, claude::STATE_AAD)
                .expect("login state opens");
        session
            .get("proxy_url")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    fn reauthorization_body(account_id: Uuid) -> Value {
        json!({
            "tenant_external_id": TENANT,
            "account_name": ACCOUNT_NAME,
            "upstream_account_id": account_id
        })
    }

    #[tokio::test]
    async fn reauthorization_inherits_account_proxy_when_field_absent() {
        let (_directory, state, account_id) = world().await;
        let operator = service_token(&state, None).await;
        let response = start(&state, &operator, reauthorization_body(account_id))
            .await
            .expect("reauthorization start");
        assert_eq!(
            started_session_proxy(&state, response).await.as_deref(),
            Some(EXISTING_PROXY)
        );
    }

    #[tokio::test]
    async fn reauthorization_replaces_account_proxy_when_url_provided() {
        let (_directory, state, account_id) = world().await;
        let operator = service_token(&state, None).await;
        let mut body = reauthorization_body(account_id);
        body["proxy_url"] = json!(REPLACEMENT_PROXY);
        let response = start(&state, &operator, body)
            .await
            .expect("reauthorization start");
        assert_eq!(
            started_session_proxy(&state, response).await.as_deref(),
            Some(REPLACEMENT_PROXY)
        );
    }

    #[tokio::test]
    async fn reauthorization_clears_account_proxy_on_explicit_null() {
        let (_directory, state, account_id) = world().await;
        let operator = service_token(&state, None).await;
        let mut body = reauthorization_body(account_id);
        body["proxy_url"] = Value::Null;
        let response = start(&state, &operator, body)
            .await
            .expect("reauthorization start");
        assert_eq!(started_session_proxy(&state, response).await, None);
    }

    #[tokio::test]
    async fn reauthorization_proxy_change_requires_a_global_operator() {
        let (_directory, state, account_id) = world().await;
        let scoped = service_token(&state, Some(TENANT)).await;
        let mut replacing = reauthorization_body(account_id);
        replacing["proxy_url"] = json!(REPLACEMENT_PROXY);
        let result = start(&state, &scoped, replacing).await;
        assert!(matches!(result, Err(AppError::Forbidden)));

        let mut clearing = reauthorization_body(account_id);
        clearing["proxy_url"] = Value::Null;
        let result = start(&state, &scoped, clearing).await;
        assert!(matches!(result, Err(AppError::Forbidden)));
    }

    #[tokio::test]
    async fn reauthorization_proxy_keeps_remote_dns_private_ip_validation() {
        let (_directory, state, account_id) = world().await;
        let operator = service_token(&state, None).await;
        let mut body = reauthorization_body(account_id);
        body["proxy_url"] = json!("socks5://100.64.0.17:1080");
        let result = start(&state, &operator, body).await;
        assert!(matches!(result, Err(AppError::BadRequest(_))));
    }
}
