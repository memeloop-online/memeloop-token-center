use super::super::*;
use crate::provider::UpstreamAccountView;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct QuotaQuery {
    tenant_external_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct ResetConfirmation {
    confirmation_token: String,
    confirmation: String,
}

fn quota_reset_idempotency_key(headers: &HeaderMap) -> Result<&str, AppError> {
    let mut values = headers.get_all("idempotency-key").iter();
    let value = values.next().ok_or_else(|| {
        AppError::BadRequest("exactly one Idempotency-Key is required for quota reset".into())
    })?;
    if values.next().is_some() {
        return Err(AppError::BadRequest(
            "exactly one Idempotency-Key is required for quota reset".into(),
        ));
    }
    let value = value.to_str().map_err(|_| {
        AppError::BadRequest(
            "Idempotency-Key must contain 1 to 200 visible ASCII characters".into(),
        )
    })?;
    if value.is_empty() || value.len() > 200 || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(AppError::BadRequest(
            "Idempotency-Key must contain 1 to 200 visible ASCII characters".into(),
        ));
    }
    Ok(value)
}

async fn reset_account(
    state: &AppState,
    headers: &HeaderMap,
    account_id: Uuid,
    query: &QuotaQuery,
) -> Result<(UpstreamAccountView, UpstreamCredential, String), AppError> {
    let service = require_service(headers, state, "providers:write").await?;
    let tenant = query.tenant_external_id.trim();
    if tenant.is_empty() || tenant.len() > 200 {
        return Err(AppError::BadRequest(
            "reset requires explicit tenant".into(),
        ));
    }
    require_service_tenant(&service, tenant)?;
    state.db.require_upstream_tenant(account_id, tenant).await?;
    let (account, credential) = state
        .db
        .upstream_account_with_credential(account_id, state.config.key_pepper.as_bytes())
        .await?;
    let actor = service
        .service_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "bootstrap".into());
    Ok((account, credential, actor))
}

pub(in crate::api) async fn prepare_quota_reset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Query(query): Query<QuotaQuery>,
) -> Result<impl IntoResponse, AppError> {
    let idempotency_key = quota_reset_idempotency_key(&headers)?;
    let (account, credential, actor) = reset_account(&state, &headers, account_id, &query).await?;
    let result = crate::upstream_quota::reset::prepare(
        &state,
        &account,
        &credential,
        query.tenant_external_id.trim(),
        &actor,
        idempotency_key,
    )
    .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(result)))
}

pub(in crate::api) async fn confirm_quota_reset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((account_id, operation_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<QuotaQuery>,
    Json(body): Json<ResetConfirmation>,
) -> Result<impl IntoResponse, AppError> {
    let idempotency_key = quota_reset_idempotency_key(&headers)?;
    if body.confirmation != "consume_one_supplier_reset_credit" {
        return Err(AppError::BadRequest(
            "confirmation must explicitly acknowledge one supplier reset credit".into(),
        ));
    }
    let (account, credential, actor) = reset_account(&state, &headers, account_id, &query).await?;
    let result = crate::upstream_quota::reset::confirm(
        &state,
        &account,
        &credential,
        query.tenant_external_id.trim(),
        &actor,
        &operation_id.to_string(),
        &body.confirmation_token,
        idempotency_key,
    )
    .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(result)))
}

pub(in crate::api) async fn reconcile_quota_reset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((account_id, operation_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<QuotaQuery>,
) -> Result<impl IntoResponse, AppError> {
    let (account, credential, actor) = reset_account(&state, &headers, account_id, &query).await?;
    let result = crate::upstream_quota::reset::reconcile(
        &state,
        &account,
        &credential,
        query.tenant_external_id.trim(),
        &actor,
        &operation_id.to_string(),
    )
    .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(result)))
}

pub(in crate::api) async fn get_quota_reset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((account_id, operation_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<QuotaQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:write").await?;
    let tenant = query.tenant_external_id.trim();
    if tenant.is_empty() || tenant.len() > 200 {
        return Err(AppError::BadRequest(
            "reset requires explicit tenant".into(),
        ));
    }
    require_service_tenant(&service, tenant)?;
    let result = state
        .db
        .quota_reset_operation_for_tenant(
            tenant,
            &account_id.to_string(),
            &operation_id.to_string(),
        )
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(result)))
}

pub(in crate::api) async fn upstream_quota(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Query(query): Query<QuotaQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:read").await?;
    let tenant = query.tenant_external_id.trim();
    if tenant.is_empty() || tenant.len() > 200 {
        return Err(AppError::BadRequest(
            "quota requires an explicit tenant".into(),
        ));
    }
    require_service_tenant(&service, tenant)?;
    state.db.require_upstream_tenant(account_id, tenant).await?;
    let (account, credential) = state
        .db
        .upstream_account_with_credential(account_id, state.config.key_pepper.as_bytes())
        .await?;
    let snapshot = state
        .upstream_quota
        .read(&state, &account, &credential, tenant)
        .await;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(snapshot)))
}
