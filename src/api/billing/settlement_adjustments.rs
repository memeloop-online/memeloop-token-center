use super::super::*;
use crate::{
    db::ReconcileSettlementAdjustmentInput,
    model::{AccountSettlementKind, micros_to_decimal_string},
};

const CLOUD_USAGE_DISCOUNT_NAMESPACE: &str = "memeloop-cloud:usage-discount";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct ReconcileSettlementAdjustmentRequest {
    namespace: String,
    request_kind: AccountSettlementKind,
    request_id: Uuid,
    currency: String,
    version: i64,
    desired_rebate: String,
    decision_digest: String,
    source: String,
}

#[derive(serde::Serialize)]
struct SettlementAdjustmentResponse {
    adjustment_entry_id: Option<Uuid>,
    event_id: Uuid,
    account_id: Uuid,
    settlement_id: Uuid,
    namespace: String,
    request_kind: AccountSettlementKind,
    request_id: Uuid,
    currency: String,
    desired_rebate: String,
    applied_delta: String,
    cumulative_rebate: String,
    remaining_rebate: String,
    version: i64,
    created_at: i64,
    replayed: bool,
}

pub(in crate::api) async fn reconcile_settlement_adjustment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((account_id, settlement_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<ReconcileSettlementAdjustmentRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "settlements:adjust").await?;
    // Do this before looking up the settlement: scoped credentials must not be
    // able to use its identifier to probe another tenant's account.
    if let Some(tenant) = service.tenant_external_id.as_deref() {
        match state.db.require_account_tenant(account_id, tenant).await {
            Ok(()) => {}
            // This endpoint intentionally makes an unbound account and an
            // account in another tenant indistinguishable.
            Err(AppError::Forbidden | AppError::NotFound) => return Err(AppError::NotFound),
            Err(error) => return Err(error),
        }
    }

    let idempotency_key = required_idempotency_key(&headers)?;
    validate_namespace(&body.namespace)?;
    validate_decision_digest(&body.decision_digest)?;
    validate_source(&body.source)?;
    if body.version <= 0 {
        return Err(AppError::BadRequest("version must be positive".into()));
    }
    let desired_rebate_micros = parse_money_micros(&body.desired_rebate, "desired_rebate")?;

    let result = state
        .db
        .reconcile_settlement_adjustment(ReconcileSettlementAdjustmentInput {
            account_id,
            settlement_id,
            namespace: body.namespace,
            request_kind: body.request_kind,
            request_id: body.request_id,
            currency: body.currency,
            desired_rebate_micros,
            version: body.version,
            decision_digest: body.decision_digest,
            source: body.source,
            idempotency_key: idempotency_key.to_owned(),
        })
        .await?;
    let status = if result.replayed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(SettlementAdjustmentResponse {
            adjustment_entry_id: result.adjustment_entry_id,
            event_id: result.event_id,
            account_id: result.account_id,
            settlement_id: result.settlement_id,
            namespace: result.namespace,
            request_kind: result.request_kind,
            request_id: result.request_id,
            currency: result.currency,
            desired_rebate: micros_to_decimal_string(result.desired_rebate_micros),
            applied_delta: micros_to_decimal_string(result.applied_delta_micros),
            cumulative_rebate: micros_to_decimal_string(result.cumulative_rebate_micros),
            remaining_rebate: micros_to_decimal_string(result.remaining_rebate_micros),
            version: result.version,
            created_at: result.created_at,
            replayed: result.replayed,
        }),
    ))
}

fn required_idempotency_key(headers: &HeaderMap) -> Result<&str, AppError> {
    let value = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AppError::BadRequest("Idempotency-Key is required".into()))?;
    if value.is_empty() || value.len() > 200 || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(AppError::BadRequest(
            "Idempotency-Key must contain 1 to 200 visible ASCII characters".into(),
        ));
    }
    Ok(value)
}

fn validate_namespace(value: &str) -> Result<(), AppError> {
    if value == CLOUD_USAGE_DISCOUNT_NAMESPACE {
        return Ok(());
    }
    let (owner, name) = value.split_once(':').ok_or_else(|| {
        AppError::BadRequest(
            "namespace must be memeloop-cloud:usage-discount or a restricted namespace".into(),
        )
    })?;
    if value.matches(':').count() != 1
        || !namespace_segment_is_valid(owner)
        || !namespace_segment_is_valid(name)
    {
        return Err(AppError::BadRequest(
            "namespace must be memeloop-cloud:usage-discount or a restricted namespace".into(),
        ));
    }
    Ok(())
}

fn namespace_segment_is_valid(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value.as_bytes()[0].is_ascii_lowercase()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn validate_decision_digest(value: &str) -> Result<(), AppError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(AppError::BadRequest(
            "decision_digest must be a lowercase 64-character SHA-256 hex digest".into(),
        ));
    }
    Ok(())
}

fn validate_source(value: &str) -> Result<(), AppError> {
    if value.trim().is_empty() || value.len() > 200 || value.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "source must contain 1 to 200 non-control characters".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjustment_namespace_is_narrow_and_stable() {
        for valid in [
            CLOUD_USAGE_DISCOUNT_NAMESPACE,
            "cloud:rebate-v1",
            "a:b",
            "namespace-1:adjustment-2",
        ] {
            assert!(validate_namespace(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "cloud",
            "Cloud:rebate",
            "cloud:Rebate",
            "cloud:rebate:next",
            ":rebate",
            "cloud:",
        ] {
            assert!(validate_namespace(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn decision_digest_must_be_lowercase_sha256_hex() {
        assert!(validate_decision_digest(&"a".repeat(64)).is_ok());
        assert!(validate_decision_digest(&"A".repeat(64)).is_err());
        assert!(validate_decision_digest("abc").is_err());
    }

    #[test]
    fn desired_rebate_decimal_parser_allows_a_zero_no_op() {
        assert_eq!(parse_money_micros("0", "desired_rebate").unwrap(), 0);
    }
}
