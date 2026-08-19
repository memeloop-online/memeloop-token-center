use super::super::*;
use super::money::parse_decimal;

#[derive(Debug, Deserialize)]
pub(in crate::api) struct GrantRequest {
    amount: String,
    source: String,
}

#[derive(Debug, Deserialize)]
pub(in crate::api) struct ReverseGrantRequest {
    grant_idempotency_key: String,
    source: String,
}

pub(in crate::api) async fn grant_balance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Json(body): Json<GrantRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "credits:write").await?;
    if let Some(tenant) = service.tenant_external_id.as_deref() {
        state.db.require_account_tenant(account_id, tenant).await?;
    }
    let amount = parse_decimal(&body.amount, "amount")?;
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::BadRequest("Idempotency-Key is required".into()))?;
    let granted = state
        .db
        .grant(account_id, amount, &body.source, idempotency_key)
        .await?;
    Ok((StatusCode::CREATED, Json(json!({"granted": granted}))))
}

pub(in crate::api) async fn reverse_grant_balance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Json(body): Json<ReverseGrantRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "credits:write").await?;
    if let Some(tenant) = service.tenant_external_id.as_deref() {
        state.db.require_account_tenant(account_id, tenant).await?;
    }
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::BadRequest("Idempotency-Key is required".into()))?;
    let reversed = state
        .db
        .reverse_grant(
            account_id,
            &body.grant_idempotency_key,
            &body.source,
            idempotency_key,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(json!({"reversed": reversed}))))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct LedgerQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    before_created_at: Option<i64>,
    before_id: Option<Uuid>,
}

pub(in crate::api) async fn list_account_ledger(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Query(query): Query<LedgerQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "credits:read").await?;
    if let Some(tenant) = service.tenant_external_id.as_deref() {
        state.db.require_account_tenant(account_id, tenant).await?;
    } else {
        state.db.require_account_exists(account_id).await?;
    }
    if !(1..=500).contains(&query.limit) {
        return Err(AppError::BadRequest(
            "limit must be between 1 and 500".into(),
        ));
    }
    let before = match (query.before_created_at, query.before_id) {
        (None, None) => None,
        (Some(created_at), Some(id)) if created_at >= 0 => Some((created_at, id)),
        (Some(_), Some(_)) => {
            return Err(AppError::BadRequest(
                "before_created_at cannot be negative".into(),
            ));
        }
        _ => {
            return Err(AppError::BadRequest(
                "before_created_at and before_id must be supplied together for ledger pagination"
                    .into(),
            ));
        }
    };
    Ok(Json(
        state
            .db
            .list_account_ledger_page(account_id, query.limit, before)
            .await?,
    ))
}
