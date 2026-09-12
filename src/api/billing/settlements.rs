use super::super::*;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct SettlementQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    after_sequence: Option<i64>,
    after_id: Option<Uuid>,
    request_id: Option<Uuid>,
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
    if query.request_id.is_some() && (query.after_sequence.is_some() || query.after_id.is_some()) {
        return Err(AppError::BadRequest(
            "request_id cannot be combined with an after cursor".into(),
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
        .list_account_settlements(account_id, query.limit, after, query.request_id)
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(page)))
}

fn default_limit() -> i64 {
    100
}
