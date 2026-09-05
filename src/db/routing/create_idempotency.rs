use serde::Serialize;
use sqlx::{Any, Row, Transaction};
use uuid::Uuid;

use super::super::{AppError, parse_uuid};
use super::RouteCreateIdempotencyKey;

/// The replay window deliberately bounds one-row-per-route-create control
/// metadata. The raw key is HMACed by the API before it reaches this module;
/// only the fixed-size digest and a non-secret request fingerprint are stored.
pub(super) const ROUTE_CREATE_IDEMPOTENCY_RETENTION_MILLIS: i64 = 30 * 24 * 60 * 60 * 1_000;
const ROUTE_CREATE_IDEMPOTENCY_PRUNE_LIMIT: i64 = 1_000;
const ROUTE_CREATE_FINGERPRINT_DOMAIN: &str = "memeloop model route create v1";

#[derive(Debug, Serialize)]
struct RouteCreateFingerprint<'a> {
    tenant_id: &'a str,
    public_model: &'a str,
    upstream_model: &'a str,
    protocol: &'a str,
    priority: i64,
    upstream_account_ids: &'a [Uuid],
    included_provider_group_ids: &'a [Uuid],
    excluded_provider_group_ids: &'a [Uuid],
    route_group_ids: &'a [Uuid],
    route_group_names: Vec<String>,
    granted_credential_ids: &'a [Uuid],
    custom_model_confirmed: bool,
}

pub(super) enum RouteCreateClaim {
    Claimed,
    Reused(Uuid),
}

/// Borrowed, already-canonicalized route-create fields used for the stable
/// operation fingerprint. Keeping this separate from the owned API input
/// avoids clone-heavy replay handling after association vectors are moved into
/// their bounded forms.
pub(super) struct RouteCreateFingerprintInput<'a> {
    pub tenant_id: &'a str,
    pub public_model: &'a str,
    pub upstream_model: &'a str,
    pub protocol: &'a str,
    pub priority: i64,
    pub custom_model_confirmed: bool,
    pub upstream_account_ids: &'a [Uuid],
    pub included_provider_group_ids: &'a [Uuid],
    pub excluded_provider_group_ids: &'a [Uuid],
    pub route_group_ids: &'a [Uuid],
    pub route_group_names: &'a [(String, String)],
    pub granted_credential_ids: &'a [Uuid],
}

pub(super) fn request_fingerprint(
    input: RouteCreateFingerprintInput<'_>,
) -> Result<String, AppError> {
    let canonical = RouteCreateFingerprint {
        tenant_id: input.tenant_id,
        public_model: input.public_model.trim(),
        upstream_model: input.upstream_model.trim(),
        protocol: input.protocol,
        priority: input.priority,
        upstream_account_ids: input.upstream_account_ids,
        included_provider_group_ids: input.included_provider_group_ids,
        excluded_provider_group_ids: input.excluded_provider_group_ids,
        route_group_ids: input.route_group_ids,
        route_group_names: input
            .route_group_names
            .iter()
            .map(|(_, normalized_name)| normalized_name.clone())
            .collect(),
        granted_credential_ids: input.granted_credential_ids,
        custom_model_confirmed: input.custom_model_confirmed,
    };
    let bytes = serde_json::to_vec(&canonical).map_err(|_| AppError::Internal)?;
    Ok(blake3::derive_key(ROUTE_CREATE_FINGERPRINT_DOMAIN, &bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Returns the owned route only when the same unexpired key claimed the same
/// canonical request. This intentionally runs before mutable-resource
/// validation: an exact replay must remain recoverable after an operator
/// changes an upstream, driver registration, or route-group name.
pub(super) async fn find_owned_route_create(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    idempotency_key: &RouteCreateIdempotencyKey,
    request_fingerprint: &str,
    now: i64,
) -> Result<Option<Uuid>, AppError> {
    delete_expired_claim_for_key(tx, tenant_id, idempotency_key, now).await?;
    let existing = sqlx::query(
        "SELECT request_fingerprint, route_id \
         FROM model_route_create_operations \
         WHERE tenant_id = $1 AND idempotency_key_hash = $2",
    )
    .bind(tenant_id)
    .bind(idempotency_key.as_bytes().to_vec())
    .fetch_optional(&mut **tx)
    .await?;
    let Some(existing) = existing else {
        return Ok(None);
    };
    let existing_fingerprint: String = existing.try_get("request_fingerprint")?;
    if existing_fingerprint != request_fingerprint {
        return Err(AppError::Conflict(
            "Idempotency-Key was already used for a different model route create".into(),
        ));
    }
    Ok(Some(parse_uuid(existing.try_get("route_id")?)?))
}

/// Atomically reserves one idempotency key for a stable route ID.
///
/// Callers hold the existing tenant routing-relation lock, which keeps the
/// multi-table route write and the replay lookup coherent. The table's primary
/// key is still the database authority if a future writer omits that lock.
pub(super) async fn claim_route_create(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    idempotency_key: &RouteCreateIdempotencyKey,
    request_fingerprint: &str,
    route_id: Uuid,
    now: i64,
) -> Result<RouteCreateClaim, AppError> {
    delete_expired_claim_for_key(tx, tenant_id, idempotency_key, now).await?;
    delete_expired_claims(tx, now).await?;
    let expires_at = now.saturating_add(ROUTE_CREATE_IDEMPOTENCY_RETENTION_MILLIS);
    let inserted = sqlx::query(
        "INSERT INTO model_route_create_operations \
         (tenant_id, idempotency_key_hash, request_fingerprint, route_id, created_at, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT(tenant_id, idempotency_key_hash) DO NOTHING",
    )
    .bind(tenant_id)
    .bind(idempotency_key.as_bytes().to_vec())
    .bind(request_fingerprint)
    .bind(route_id.to_string())
    .bind(now)
    .bind(expires_at)
    .execute(&mut **tx)
    .await?;
    if inserted.rows_affected() == 1 {
        return Ok(RouteCreateClaim::Claimed);
    }

    find_owned_route_create(tx, tenant_id, idempotency_key, request_fingerprint, now)
        .await?
        .map(RouteCreateClaim::Reused)
        .ok_or(AppError::Internal)
}

async fn delete_expired_claim_for_key(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    idempotency_key: &RouteCreateIdempotencyKey,
    now: i64,
) -> Result<(), AppError> {
    // The bounded global sweep below may have a backlog. Always remove this
    // operation's expired row first so the 30-day replay window is a hard
    // contract rather than a best-effort cleanup target.
    sqlx::query(
        "DELETE FROM model_route_create_operations \
         WHERE tenant_id = $1 AND idempotency_key_hash = $2 AND expires_at <= $3",
    )
    .bind(tenant_id)
    .bind(idempotency_key.as_bytes().to_vec())
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn delete_expired_claims(tx: &mut Transaction<'_, Any>, now: i64) -> Result<(), AppError> {
    // Both supported SQL engines accept this bounded keyset delete. It avoids
    // turning a normal control write into an unbounded retention sweep.
    sqlx::query(
        "DELETE FROM model_route_create_operations \
         WHERE (tenant_id, idempotency_key_hash) IN ( \
             SELECT tenant_id, idempotency_key_hash \
             FROM model_route_create_operations \
             WHERE expires_at <= $1 \
             ORDER BY expires_at, tenant_id, idempotency_key_hash \
             LIMIT $2 \
         )",
    )
    .bind(now)
    .bind(ROUTE_CREATE_IDEMPOTENCY_PRUNE_LIMIT)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
