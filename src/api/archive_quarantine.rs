use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::IntoResponse,
};
use serde::Deserialize;
use uuid::Uuid;

use super::{require_global_service, require_service};
use crate::{
    AppState,
    db::{SessionArchiveQuarantineFilter, SessionArchiveQuarantineResolutionInput},
    error::AppError,
    model::AuthenticatedService,
};

const QUARANTINE_READ_SCOPE: &str = "imports:session_archive:quarantine:read";
const QUARANTINE_RESOLVE_SCOPE: &str = "imports:session_archive:quarantine:resolve";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct QuarantineListQuery {
    tenant_external_id: String,
    state: Option<String>,
    #[serde(default = "default_page_limit")]
    limit: i64,
    before_started_at: Option<i64>,
    before_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct QuarantineDetailQuery {
    tenant_external_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct ResolveQuarantineRequest {
    tenant_external_id: String,
    action: String,
    key_id: Option<Uuid>,
    expected_record_digest: String,
    evidence_digest: String,
    note: Option<String>,
}

fn default_page_limit() -> i64 {
    100
}

pub(in crate::api) async fn list_archive_quarantine(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<QuarantineListQuery>,
) -> Result<impl IntoResponse, AppError> {
    let _ = persistent_global_service(&headers, &state, QUARANTINE_READ_SCOPE).await?;
    validate_tenant_external_id(&query.tenant_external_id)?;
    if !(1..=100).contains(&query.limit) {
        return Err(AppError::BadRequest(
            "quarantine page limit must be between 1 and 100".into(),
        ));
    }
    if query.before_started_at.is_some() != query.before_id.is_some() {
        return Err(AppError::BadRequest(
            "before_started_at and before_id must be supplied together".into(),
        ));
    }
    Ok(Json(
        state
            .db
            .list_session_archive_quarantine(SessionArchiveQuarantineFilter {
                tenant_external_id: &query.tenant_external_id,
                state: query.state.as_deref(),
                limit: query.limit,
                before_started_at: query.before_started_at,
                before_id: query.before_id,
            })
            .await?,
    ))
}

pub(in crate::api) async fn get_archive_quarantine(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(quarantine_id): Path<Uuid>,
    Query(query): Query<QuarantineDetailQuery>,
) -> Result<impl IntoResponse, AppError> {
    let _ = persistent_global_service(&headers, &state, QUARANTINE_READ_SCOPE).await?;
    validate_tenant_external_id(&query.tenant_external_id)?;
    Ok(Json(
        state
            .db
            .get_session_archive_quarantine(&query.tenant_external_id, quarantine_id)
            .await?,
    ))
}

pub(in crate::api) async fn resolve_archive_quarantine(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(quarantine_id): Path<Uuid>,
    Json(body): Json<ResolveQuarantineRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = persistent_global_service(&headers, &state, QUARANTINE_RESOLVE_SCOPE).await?;
    validate_tenant_external_id(&body.tenant_external_id)?;
    let idempotency_key = required_idempotency_key(&headers)?;
    Ok(Json(
        state
            .db
            .resolve_session_archive_quarantine(SessionArchiveQuarantineResolutionInput {
                tenant_external_id: &body.tenant_external_id,
                quarantine_id,
                action: &body.action,
                key_id: body.key_id,
                expected_record_digest: &body.expected_record_digest,
                evidence_digest: &body.evidence_digest,
                note: body.note.as_deref(),
                idempotency_key,
                resolved_by_service_id: service
                    .service_id
                    .expect("persistent service identity was checked"),
            })
            .await?,
    ))
}

async fn persistent_global_service(
    headers: &HeaderMap,
    state: &AppState,
    scope: &str,
) -> Result<AuthenticatedService, AppError> {
    let service = require_service(headers, state, scope).await?;
    require_global_service(&service)?;
    if service.service_id.is_none() {
        // The bootstrap secret is intentionally excluded from evidence-bearing
        // operator decisions: every resolution must retain a stable actor ID.
        return Err(AppError::Forbidden);
    }
    Ok(service)
}

fn validate_tenant_external_id(tenant: &str) -> Result<(), AppError> {
    if tenant.is_empty() || tenant.len() > 200 || tenant.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(AppError::BadRequest(
            "tenant_external_id must contain 1 to 200 non-control characters".into(),
        ));
    }
    Ok(())
}

fn required_idempotency_key(headers: &HeaderMap) -> Result<&str, AppError> {
    let mut values = headers.get_all("idempotency-key").iter();
    let value = values
        .next()
        .and_then(|value| value.to_str().ok())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 200
                && value.bytes().all(|byte| byte.is_ascii_graphic())
        })
        .ok_or_else(|| {
            AppError::BadRequest(
                "Idempotency-Key must contain 1 to 200 visible ASCII characters".into(),
            )
        })?;
    if values.next().is_some() {
        return Err(AppError::BadRequest(
            "exactly one Idempotency-Key is required".into(),
        ));
    }
    Ok(value)
}
