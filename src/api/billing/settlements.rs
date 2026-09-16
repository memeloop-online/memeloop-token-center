use super::super::*;
use crate::model::AccountSettlementKind;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct SettlementQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    after_sequence: Option<i64>,
    after_id: Option<Uuid>,
    request_kind: Option<AccountSettlementKind>,
    request_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct SettlementCorrectionPreviewQuery {
    from_created_at: Option<i64>,
    to_created_at: Option<i64>,
    #[serde(default = "default_limit")]
    limit: i64,
    after_created_at: Option<i64>,
    after_request_id: Option<Uuid>,
}

pub(in crate::api) async fn list_account_settlements(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Query(query): Query<SettlementQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "credits:read").await?;
    if !service.allows("requests:read") {
        return Err(AppError::Forbidden);
    }
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
    let exact = match (query.request_kind, query.request_id) {
        (Some(kind), Some(id)) => Some((kind, id)),
        (None, None) => None,
        _ => {
            return Err(AppError::BadRequest(
                "request_kind and request_id must be supplied together".into(),
            ));
        }
    };
    if exact.is_some() && (query.after_sequence.is_some() || query.after_id.is_some()) {
        return Err(AppError::BadRequest(
            "exact settlement lookup cannot be combined with an after cursor".into(),
        ));
    }
    let after = match (query.after_sequence, query.after_id) {
        (None, None) => None,
        (Some(sequence), Some(id)) if sequence > 0 => Some((sequence, id)),
        (Some(_), Some(_)) => {
            return Err(AppError::BadRequest(
                "after_sequence must be positive".into(),
            ));
        }
        _ => {
            return Err(AppError::BadRequest(
                "after_sequence and after_id must be supplied together".into(),
            ));
        }
    };
    let page = state
        .db
        .list_account_settlements(account_id, query.limit, after, exact)
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(page)))
}

pub(in crate::api) async fn list_settlement_correction_previews(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account_id): Path<Uuid>,
    Query(query): Query<SettlementCorrectionPreviewQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "settlements:adjust").await?;
    if let Some(tenant) = service.tenant_external_id.as_deref() {
        state.db.require_account_tenant(account_id, tenant).await?;
    } else {
        state.db.require_account_exists(account_id).await?;
    }
    let from_created_at = query.from_created_at.ok_or_else(|| {
        AppError::BadRequest("settlement correction preview requires from_created_at".into())
    })?;
    let to_created_at = query.to_created_at.ok_or_else(|| {
        AppError::BadRequest("settlement correction preview requires to_created_at".into())
    })?;
    if !(1..=500).contains(&query.limit) {
        return Err(AppError::BadRequest(
            "limit must be between 1 and 500".into(),
        ));
    }
    let after = match (query.after_created_at, query.after_request_id) {
        (None, None) => None,
        (Some(created_at), Some(request_id)) if created_at >= 0 => Some((created_at, request_id)),
        (Some(_), Some(_)) => {
            return Err(AppError::BadRequest(
                "after_created_at must be non-negative".into(),
            ));
        }
        _ => {
            return Err(AppError::BadRequest(
                "after_created_at and after_request_id must be supplied together".into(),
            ));
        }
    };
    let page = state
        .db
        .list_settlement_correction_previews(
            account_id,
            from_created_at,
            to_created_at,
            query.limit,
            after,
        )
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(page)))
}

fn default_limit() -> i64 {
    100
}
