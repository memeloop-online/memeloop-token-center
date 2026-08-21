use super::super::*;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct CreateKeyRequest {
    #[serde(default = "default_tenant")]
    tenant_external_id: String,
    principal_external_id: String,
    alias: String,
    #[serde(default = "default_currency")]
    currency: String,
    #[serde(default)]
    policy: KeyPolicyInput,
    #[serde(default = "zero_amount")]
    initial_balance: String,
    #[serde(default)]
    route_ids: Vec<Uuid>,
    #[serde(default)]
    route_group_ids: Vec<Uuid>,
}

pub(in crate::api) fn default_tenant() -> String {
    "default".to_owned()
}

pub(in crate::api) fn default_currency() -> String {
    "USD".to_owned()
}

fn zero_amount() -> String {
    "0".to_owned()
}

pub(in crate::api) async fn create_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateKeyRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "keys:write").await?;
    require_service_tenant(&service, &body.tenant_external_id)?;
    let initial_balance = Decimal::from_str(&body.initial_balance)
        .map_err(|_| AppError::BadRequest("initial_balance must be a decimal string".into()))?;
    if initial_balance.is_sign_negative() {
        return Err(AppError::BadRequest(
            "initial_balance cannot be negative".into(),
        ));
    }
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let issued = state
        .db
        .create_key_with_routing(
            CreateKeyInput {
                tenant_external_id: body.tenant_external_id,
                principal_external_id: body.principal_external_id,
                alias: body.alias,
                currency: body.currency,
                policy: body.policy.into(),
                initial_balance,
                idempotency_key,
            },
            &body.route_ids,
            &body.route_group_ids,
            state.config.key_pepper.as_bytes(),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(issued)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct KeysQuery {
    tenant_external_id: Option<String>,
    principal_external_id: Option<String>,
    #[serde(default = "default_key_list_limit")]
    limit: i64,
    before_created_at: Option<i64>,
    before_id: Option<Uuid>,
}

fn default_key_list_limit() -> i64 {
    // Preserve the pre-pagination response size for existing operator clients;
    // new callers should request smaller pages and continue with the cursor.
    500
}

pub(in crate::api) async fn list_keys(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<KeysQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "keys:read").await?;
    let tenant = management_tenant(&service, query.tenant_external_id)?;
    let principal = query
        .principal_external_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if principal.is_some_and(|value| value.len() > 200 || value.chars().any(char::is_control)) {
        return Err(AppError::BadRequest(
            "principal_external_id must contain at most 200 non-control characters".into(),
        ));
    }
    let before = list_cursor(
        query.limit,
        query.before_created_at,
        query.before_id,
        "credential",
    )?;
    Ok(Json(
        state
            .db
            .list_managed_keys_page(tenant.as_deref(), principal, query.limit, before)
            .await?,
    ))
}

fn list_cursor(
    limit: i64,
    before_created_at: Option<i64>,
    before_id: Option<Uuid>,
    resource: &str,
) -> Result<Option<(i64, Uuid)>, AppError> {
    if !(1..=500).contains(&limit) {
        return Err(AppError::BadRequest(
            "limit must be between 1 and 500".into(),
        ));
    }
    match (before_created_at, before_id) {
        (None, None) => Ok(None),
        (Some(created_at), Some(id)) if created_at >= 0 => Ok(Some((created_at, id))),
        (Some(_), Some(_)) => Err(AppError::BadRequest(
            "before_created_at cannot be negative".into(),
        )),
        _ => Err(AppError::BadRequest(format!(
            "before_created_at and before_id must be supplied together for {resource} pagination"
        ))),
    }
}

pub(in crate::api) async fn rotate_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "keys:write").await?;
    let idempotency_key = headers
        .get("idempotency-key")
        .ok_or_else(|| AppError::BadRequest("Idempotency-Key is required".into()))?
        .to_str()
        .map_err(|_| AppError::BadRequest("Idempotency-Key must be valid ASCII".into()))?;
    if let Some(tenant) = service.tenant_external_id.as_deref() {
        state.db.require_key_tenant(key_id, tenant).await?;
    }
    let issued = state
        .db
        .rotate_key(key_id, idempotency_key, state.config.key_pepper.as_bytes())
        .await?;
    Ok(Json(issued))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct RenameKeyRequest {
    alias: String,
}

pub(in crate::api) async fn rename_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key_id): Path<Uuid>,
    Json(body): Json<RenameKeyRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "keys:write").await?;
    if let Some(tenant) = service.tenant_external_id.as_deref() {
        state.db.require_key_tenant(key_id, tenant).await?;
    }
    Ok(Json(state.db.rename_key(key_id, &body.alias).await?))
}

pub(in crate::api) async fn key_limits(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "keys:read").await?;
    if let Some(tenant) = service.tenant_external_id.as_deref() {
        state.db.require_key_tenant(key_id, tenant).await?;
    }
    Ok(Json(state.db.key_limit_snapshot(key_id).await?))
}

pub(in crate::api) async fn update_key_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key_id): Path<Uuid>,
    Json(policy): Json<KeyPolicyInput>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "keys:write").await?;
    if let Some(tenant) = service.tenant_external_id.as_deref() {
        state.db.require_key_tenant(key_id, tenant).await?;
    }
    Ok(Json(
        state.db.update_key_policy(key_id, policy.into()).await?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct StatusRequest {
    pub(super) status: String,
}

pub(in crate::api) async fn set_key_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key_id): Path<Uuid>,
    Json(body): Json<StatusRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "keys:write").await?;
    if let Some(tenant) = service.tenant_external_id.as_deref() {
        state.db.require_key_tenant(key_id, tenant).await?;
    }
    Ok(Json(json!({
        "key_id": key_id,
        "status": state.db.set_key_status(key_id, &body.status).await?
    })))
}
