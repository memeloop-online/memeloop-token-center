use crate::{
    AppState,
    db::{
        ImageGenerationQuarantineResolution, ImageGenerationQuarantineView,
        ResolveImageGenerationQuarantine,
    },
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
    confirmed_cost_micros: i64,
    currency: String,
    evidence_digest: String,
}

async fn actor(
    headers: &HeaderMap,
    state: &AppState,
    tenant: &str,
    scope: &str,
) -> Result<Uuid, AppError> {
    let service = super::require_service(headers, state, scope).await?;
    if service.tenant_external_id.as_deref() != Some(tenant) {
        return Err(AppError::Forbidden);
    }
    service.service_id.ok_or(AppError::Forbidden)
}

pub(super) async fn list_image_generation_quarantine(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<ImageGenerationQuarantineView>>, AppError> {
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
            .list_image_generation_quarantine(
                &query.tenant_external_id,
                query.limit,
                query.after_id,
            )
            .await?,
    ))
}

pub(super) async fn get_image_generation_quarantine(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(request_id): Path<Uuid>,
    Query(query): Query<DetailQuery>,
) -> Result<Json<ImageGenerationQuarantineView>, AppError> {
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
            .image_generation_quarantine(&query.tenant_external_id, request_id)
            .await?,
    ))
}

fn idempotency_hash(headers: &HeaderMap, pepper: &[u8]) -> Result<String, AppError> {
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
    let hash_key = blake3::derive_key("image-generation-quarantine-idempotency-v1", pepper);
    Ok(blake3::keyed_hash(&hash_key, key.as_bytes())
        .to_hex()
        .to_string())
}

pub(super) async fn resolve_image_generation_quarantine(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(request_id): Path<Uuid>,
    Json(body): Json<ResolveRequest>,
) -> Result<Json<ImageGenerationQuarantineResolution>, AppError> {
    let actor_service_id = actor(
        &headers,
        &state,
        &body.tenant_external_id,
        "generations:reconcile",
    )
    .await?;
    let idempotency_hash = idempotency_hash(&headers, state.config.key_pepper.as_bytes())?;
    Ok(Json(
        state
            .db
            .resolve_image_generation_quarantine(ResolveImageGenerationQuarantine {
                tenant_external_id: &body.tenant_external_id,
                request_id,
                actor_service_id,
                idempotency_hash: &idempotency_hash,
                expected_revision: &body.expected_revision,
                action: &body.action,
                confirmed_cost_micros: body.confirmed_cost_micros,
                currency: &body.currency,
                evidence_digest: &body.evidence_digest,
            })
            .await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn reconciliation_key_is_single_bounded_visible_ascii_and_domain_separated() {
        let mut headers = HeaderMap::new();
        assert!(idempotency_hash(&headers, b"pepper").is_err());
        for invalid in ["", "two words", "\t", &"a".repeat(201)] {
            headers.insert("idempotency-key", HeaderValue::from_str(invalid).unwrap());
            assert!(idempotency_hash(&headers, b"pepper").is_err());
        }
        headers.insert("idempotency-key", HeaderValue::from_static("evidence-1"));
        let hash = idempotency_hash(&headers, b"pepper").unwrap();
        assert_eq!(hash, idempotency_hash(&headers, b"pepper").unwrap());
        let old_key = blake3::derive_key("generation-quarantine-idempotency-v1", b"pepper");
        assert_ne!(
            hash,
            blake3::keyed_hash(&old_key, b"evidence-1")
                .to_hex()
                .to_string()
        );
        headers.append("idempotency-key", HeaderValue::from_static("second"));
        assert!(idempotency_hash(&headers, b"pepper").is_err());
    }

    #[test]
    fn resolution_body_rejects_unknown_fields_and_non_integer_cost() {
        let body = serde_json::json!({"tenant_external_id":"tenant", "expected_revision":"revision", "action":"not_delivered", "confirmed_cost_micros":0, "currency":"USD", "evidence_digest":"evidence"});
        assert!(serde_json::from_value::<ResolveRequest>(body.clone()).is_ok());
        let mut extra = body.clone();
        extra["retry"] = true.into();
        assert!(serde_json::from_value::<ResolveRequest>(extra).is_err());
        for cost in [
            serde_json::Value::Null,
            serde_json::json!(0.5),
            serde_json::json!("0"),
        ] {
            let mut invalid = body.clone();
            invalid["confirmed_cost_micros"] = cost;
            assert!(serde_json::from_value::<ResolveRequest>(invalid).is_err());
        }
    }
}
