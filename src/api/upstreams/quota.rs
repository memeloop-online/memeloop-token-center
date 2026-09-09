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
    let (account, credential, actor) = reset_account(&state, &headers, account_id, &query).await?;
    let result = crate::upstream_quota::reset::prepare(
        &state,
        &account,
        &credential,
        query.tenant_external_id.trim(),
        &actor,
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
    let (account, credential, actor) = reset_account(&state, &headers, account_id, &query).await?;
    let result = crate::upstream_quota::reset::confirm(
        &state,
        &account,
        &credential,
        query.tenant_external_id.trim(),
        &actor,
        &operation_id.to_string(),
        &body.confirmation_token,
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
    let (account, credential, _) = reset_account(&state, &headers, account_id, &query).await?;
    let result = crate::upstream_quota::reset::reconcile(
        &state,
        &account,
        &credential,
        query.tenant_external_id.trim(),
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
