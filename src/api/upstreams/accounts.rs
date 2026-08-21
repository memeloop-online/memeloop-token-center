use super::super::*;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct CreateUpstreamRequest {
    #[serde(default = "default_tenant")]
    tenant_external_id: String,
    name: String,
    #[serde(default = "default_provider_driver")]
    driver: String,
    config: Value,
    /// Kept as raw JSON until the provider-owned schema has validated it.
    /// Serde defaults belong to the core credential ABI and must not become
    /// undeclared provider fields before provider validation.
    credential: Value,
}

pub(super) fn default_provider_driver() -> String {
    "http-json".to_owned()
}

pub(in crate::api) async fn create_upstream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateUpstreamRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:write").await?;
    require_service_tenant(&service, &body.tenant_external_id)?;
    if !state.providers.is_public(&body.driver) {
        return Err(AppError::BadRequest("unknown provider driver".into()));
    }
    if !state.providers.supports_direct_creation(&body.driver) {
        return Err(AppError::BadRequest(
            "this upstream must be connected with its authorization flow".into(),
        ));
    }
    validate_provider_config_schema(&state, &body.driver, &body.config)?;
    validate_provider_credential_schema(&state, &body.driver, &body.credential)?;
    let credential: UpstreamCredential = serde_json::from_value(body.credential)
        .map_err(|error| AppError::BadRequest(format!("invalid upstream credential: {error}")))?;
    validate_upstream_destination(&body.driver, &body.config, &service, &state).await?;
    credential.validate(unix_millis())?;
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: body.tenant_external_id,
                name: body.name,
                driver: body.driver,
                config: body.config,
                credential,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await?;
    super::trigger_upstream_model_sync(state, account.id);
    Ok((StatusCode::CREATED, Json(account)))
}

pub(super) fn validate_provider_schema(
    state: &AppState,
    driver: &str,
    config: &Value,
    credential: &UpstreamCredential,
) -> Result<(), AppError> {
    validate_provider_config_schema(state, driver, config)?;
    validate_provider_credential_schema_from_canonical(state, driver, credential)
}

pub(super) fn validate_provider_credential_schema(
    state: &AppState,
    driver: &str,
    credential: &Value,
) -> Result<(), AppError> {
    let provider = state
        .providers
        .get(driver)
        .ok_or_else(|| AppError::BadRequest(format!("unknown provider driver: {driver}")))?;
    crate::schema::validate_instance(&provider.credential_schema, credential)
}

/// OAuth flows produce a canonical core credential with default header fields.
/// Provider schemas describe their external credential shape, so first accept
/// a schema that explicitly declares those fields, then retry with core-owned
/// defaults removed. Custom non-default header behaviour is never stripped.
fn validate_provider_credential_schema_from_canonical(
    state: &AppState,
    driver: &str,
    credential: &UpstreamCredential,
) -> Result<(), AppError> {
    let mut value = serde_json::to_value(credential).map_err(|_| AppError::Internal)?;
    if validate_provider_credential_schema(state, driver, &value).is_ok() {
        return Ok(());
    }
    if let Some(object) = value.as_object_mut() {
        if object.get("header").and_then(Value::as_str) == Some("authorization") {
            object.remove("header");
        }
        if object.get("prefix").and_then(Value::as_str) == Some("Bearer ") {
            object.remove("prefix");
        }
    }
    validate_provider_credential_schema(state, driver, &value)
}

pub(super) fn validate_provider_config_schema(
    state: &AppState,
    driver: &str,
    config: &Value,
) -> Result<(), AppError> {
    let provider = state
        .providers
        .get(driver)
        .ok_or_else(|| AppError::BadRequest(format!("unknown provider driver: {driver}")))?;
    crate::schema::validate_instance(&provider.config_schema, config)
}

pub(super) async fn validate_upstream_destination(
    driver: &str,
    config: &Value,
    service: &AuthenticatedService,
    state: &AppState,
) -> Result<(), AppError> {
    let base_url = validate_config(config)?;
    let scope = network::scope_from_config(config);
    if scope == OutboundScope::Private {
        require_global_service(service)?;
    }
    // Building the operation client validates and pins every public DNS answer.
    let _ = network::client_for_url(
        &state.http,
        &base_url,
        scope,
        state.config.allow_oauth_loopback,
    )
    .await?;
    validate_secondary_outbound_urls(config, scope, state).await?;
    validate_provider_config(driver, config)
}

async fn validate_secondary_outbound_urls(
    config: &Value,
    scope: OutboundScope,
    state: &AppState,
) -> Result<(), AppError> {
    if let Some(refresh_url) = config.pointer("/oauth/refresh_url").and_then(Value::as_str) {
        let oauth_scope = if config.pointer("/oauth/driver").and_then(Value::as_str)
            == Some("provider_adapter")
        {
            OutboundScope::Private
        } else {
            OutboundScope::Public
        };
        let _ = network::client_for_url(
            &state.http,
            refresh_url,
            oauth_scope,
            state.config.allow_oauth_loopback,
        )
        .await?;
    }
    for result_origin in config
        .get("result_origins")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let parsed = network::checked_http_url(result_origin)?;
        if parsed.origin().ascii_serialization() != result_origin || parsed.path() != "/" {
            return Err(AppError::BadRequest(
                "generation result_origins must be exact HTTP(S) origins".into(),
            ));
        }
        let _ = network::client_for_url(
            &state.http,
            result_origin,
            scope,
            state.config.allow_oauth_loopback,
        )
        .await?;
    }
    Ok(())
}

fn validate_provider_config(driver: &str, config: &Value) -> Result<(), AppError> {
    if driver == "comfyui"
        && (config
            .get("workflow_id")
            .and_then(Value::as_str)
            .is_none_or(|value| value.trim().is_empty() || value.len() > 200)
            || !config
                .get("workflow_template")
                .is_some_and(Value::is_object))
    {
        return Err(AppError::BadRequest(format!(
            "{driver} requires an administrator-owned workflow_id and workflow_template"
        )));
    }
    Ok(())
}

pub(in crate::api) async fn list_upstreams(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<UpstreamListQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:read").await?;
    let tenant = management_tenant(&service, query.tenant_external_id)?;
    let values = state
        .db
        .list_upstream_accounts_page(
            tenant.as_deref(),
            query.before_created_at,
            query.before_id,
            query.limit,
        )
        .await?;
    Ok(Json(values))
}

#[derive(Debug, Deserialize)]
pub(in crate::api) struct UpstreamListQuery {
    tenant_external_id: Option<String>,
    before_created_at: Option<i64>,
    before_id: Option<Uuid>,
    #[serde(default = "default_upstream_list_limit")]
    limit: i64,
}

fn default_upstream_list_limit() -> i64 {
    100
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct UpdateUpstreamRequest {
    tenant_external_id: String,
    name: String,
    config: Value,
    expected_updated_at: i64,
}

pub(in crate::api) async fn update_upstream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Json(body): Json<UpdateUpstreamRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:write").await?;
    require_service_tenant(&service, &body.tenant_external_id)?;
    state
        .db
        .require_upstream_tenant(account_id, &body.tenant_external_id)
        .await?;
    let driver = state.db.upstream_driver(account_id).await?;
    validate_provider_config_schema(&state, &driver, &body.config)?;
    validate_upstream_destination(&driver, &body.config, &service, &state).await?;
    let account = state
        .db
        .update_upstream_account(
            account_id,
            &body.tenant_external_id,
            UpdateUpstreamAccountInput {
                name: body.name,
                config: body.config,
                expected_updated_at: body.expected_updated_at,
            },
        )
        .await?;
    super::trigger_upstream_model_sync(state, account_id);
    Ok(Json(account))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct SetUpstreamStatusRequest {
    tenant_external_id: String,
    status: String,
    expected_updated_at: i64,
}

pub(in crate::api) async fn set_upstream_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Json(body): Json<SetUpstreamStatusRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:write").await?;
    require_service_tenant(&service, &body.tenant_external_id)?;
    state
        .db
        .require_upstream_tenant(account_id, &body.tenant_external_id)
        .await?;
    if body.status == "active" {
        let (_, credential) = state
            .db
            .upstream_account_with_credential(account_id, state.config.key_pepper.as_bytes())
            .await?;
        credential.validate(unix_millis())?;
    }
    let should_sync = body.status == "active";
    let account = state
        .db
        .set_upstream_account_status(
            account_id,
            &body.tenant_external_id,
            &body.status,
            body.expected_updated_at,
        )
        .await?;
    if should_sync {
        super::trigger_upstream_model_sync(state, account_id);
    }
    Ok(Json(account))
}

#[derive(Debug, Deserialize)]
pub(in crate::api) struct DeleteUpstreamQuery {
    tenant_external_id: String,
    expected_updated_at: i64,
}

pub(in crate::api) async fn delete_upstream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Query(query): Query<DeleteUpstreamQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:write").await?;
    require_service_tenant(&service, &query.tenant_external_id)?;
    state
        .db
        .delete_upstream_account(
            account_id,
            &query.tenant_external_id,
            query.expected_updated_at,
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub(in crate::api) struct RotateUpstreamCredentialRequest {
    credential: Value,
}

pub(in crate::api) async fn rotate_upstream_credential(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Json(body): Json<RotateUpstreamCredentialRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:write").await?;
    let idempotency_key = headers
        .get("idempotency-key")
        .ok_or_else(|| AppError::BadRequest("Idempotency-Key is required".into()))?
        .to_str()
        .map_err(|_| AppError::BadRequest("Idempotency-Key must be valid ASCII".into()))?;
    if let Some(tenant) = service.tenant_external_id.as_deref() {
        state.db.require_upstream_tenant(account_id, tenant).await?;
    }
    let driver = state.db.upstream_driver(account_id).await?;
    let provider = state
        .providers
        .get(&driver)
        .ok_or_else(|| AppError::BadRequest(format!("unknown provider driver: {driver}")))?;
    crate::schema::validate_instance(&provider.credential_schema, &body.credential)?;
    let credential: UpstreamCredential = serde_json::from_value(body.credential)
        .map_err(|error| AppError::BadRequest(format!("invalid upstream credential: {error}")))?;
    credential.validate(unix_millis())?;
    let (account, changed) = state
        .db
        .rotate_upstream_credential_with_outcome(
            account_id,
            credential,
            idempotency_key,
            state.config.key_pepper.as_bytes(),
        )
        .await?;
    if changed {
        super::trigger_upstream_model_sync(state, account_id);
    }
    Ok(Json(account))
}
