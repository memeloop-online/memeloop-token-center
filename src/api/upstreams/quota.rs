use super::super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct QuotaQuery {
    tenant_external_id: String,
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
