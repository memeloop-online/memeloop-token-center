use crate::{
    AppState,
    db::{GenerationQuarantineResolution, GenerationQuarantineView, ResolveGenerationQuarantine},
    error::AppError,
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
};
use serde::Deserialize;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListQuery {
    tenant_external_id: String,
    #[serde(default = "default_limit")]
    limit: i64,
    after_id: Option<Uuid>,
}

fn default_limit() -> i64 {
    100
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DetailQuery {
    tenant_external_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ResolveRequest {
    tenant_external_id: String,
    expected_revision: String,
    action: String,
    #[serde(default, deserialize_with = "present_upstream_id")]
    upstream_job_id: Option<String>,
    evidence_digest: String,
}

fn present_upstream_id<'de, D: serde::Deserializer<'de>>(
    value: D,
) -> Result<Option<String>, D::Error> {
    String::deserialize(value).map(Some)
}

async fn actor(
    headers: &HeaderMap,
    state: &AppState,
    tenant: &str,
    scope: &str,
) -> Result<Uuid, AppError> {
    let service = super::require_service(headers, state, scope).await?;
    // Both bootstrap and unbounded global credentials are excluded from these
    // evidence-bearing decisions. The audit actor must be persistent and bound
    // to precisely the requested tenant, not merely a broad write capability.
    if service.tenant_external_id.as_deref() != Some(tenant) {
        return Err(AppError::Forbidden);
    }
    service.service_id.ok_or(AppError::Forbidden)
}

pub(super) async fn list_generation_quarantine(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<GenerationQuarantineView>>, AppError> {
    actor(
        &headers,
        &state,
        &query.tenant_external_id,
        "generations:quarantine:read",
    )
    .await?;
    Ok(Json(
        state
            .db
            .list_generation_quarantine(&query.tenant_external_id, query.limit, query.after_id)
            .await?,
    ))
}

pub(super) async fn get_generation_quarantine(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(job_id): Path<Uuid>,
    Query(query): Query<DetailQuery>,
) -> Result<Json<GenerationQuarantineView>, AppError> {
    actor(
        &headers,
        &state,
        &query.tenant_external_id,
        "generations:quarantine:read",
    )
    .await?;
    Ok(Json(
        state
            .db
            .generation_quarantine(&query.tenant_external_id, job_id)
            .await?,
    ))
}

pub(super) async fn resolve_generation_quarantine(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(job_id): Path<Uuid>,
    Json(body): Json<ResolveRequest>,
) -> Result<Json<GenerationQuarantineResolution>, AppError> {
    let actor_service_id = actor(
        &headers,
        &state,
        &body.tenant_external_id,
        "generations:reconcile",
    )
    .await?;
    let mut values = headers.get_all("idempotency-key").iter();
    let key = values
        .next()
        .and_then(|value| value.to_str().ok())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 200
                && value.bytes().all(|byte| byte.is_ascii_graphic())
        })
        .ok_or_else(|| {
            AppError::BadRequest(
                "exactly one visible ASCII Idempotency-Key of 1 to 200 characters is required"
                    .into(),
            )
        })?;
    if values.next().is_some() {
        return Err(AppError::BadRequest(
            "exactly one Idempotency-Key is required".into(),
        ));
    }
    let hash_key = blake3::derive_key(
        "generation-quarantine-idempotency-v1",
        state.config.key_pepper.as_bytes(),
    );
    let idempotency_hash = blake3::keyed_hash(&hash_key, key.as_bytes())
        .to_hex()
        .to_string();
    Ok(Json(
        state
            .db
            .resolve_generation_quarantine(ResolveGenerationQuarantine {
                tenant_external_id: &body.tenant_external_id,
                job_id,
                actor_service_id,
                idempotency_hash: &idempotency_hash,
                expected_revision: &body.expected_revision,
                action: &body.action,
                upstream_job_id: body.upstream_job_id.as_deref(),
                evidence_digest: &body.evidence_digest,
            })
            .await?,
    ))
}
