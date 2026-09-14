use sha2::{Digest, Sha256};

use super::super::super::*;

pub(super) fn settlement_kind_name(kind: AccountSettlementKind) -> &'static str {
    match kind {
        AccountSettlementKind::Text => "text",
        AccountSettlementKind::Generation => "generation",
    }
}

pub(super) fn canonicalize_input(input: &mut ReconcileSettlementAdjustmentInput) {
    input.namespace = input.namespace.trim().to_owned();
    input.currency = input.currency.trim().to_ascii_uppercase();
    input.decision_digest = input.decision_digest.trim().to_ascii_lowercase();
    input.source = input.source.trim().to_owned();
    input.idempotency_key = input.idempotency_key.trim().to_owned();
}

pub(super) fn validate_input(input: &ReconcileSettlementAdjustmentInput) -> Result<(), AppError> {
    validate_idempotency_key(&input.idempotency_key, "Idempotency-Key")?;
    validate_currency(&input.currency)?;
    let mut namespace_parts = input.namespace.split(':');
    let namespace_owner = namespace_parts.next().unwrap_or_default();
    let namespace_name = namespace_parts.next().unwrap_or_default();
    if namespace_parts.next().is_some()
        || !valid_namespace_part(namespace_owner)
        || !valid_namespace_part(namespace_name)
    {
        return Err(AppError::BadRequest(
            "namespace must match [a-z][a-z0-9-]{0,63}:[a-z][a-z0-9-]{0,63}".into(),
        ));
    }
    if input.version <= 0 || input.desired_rebate_micros < 0 {
        return Err(AppError::BadRequest(
            "desired rebate must be non-negative and version must be positive".into(),
        ));
    }
    if input.decision_digest.len() != 64
        || !input
            .decision_digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(AppError::BadRequest(
            "decision_digest must be a lowercase SHA-256 hex digest".into(),
        ));
    }
    if input.source.is_empty()
        || input.source.len() > 200
        || input.source.chars().any(char::is_control)
    {
        return Err(AppError::BadRequest(
            "source must contain 1 to 200 non-control characters".into(),
        ));
    }
    Ok(())
}

fn valid_namespace_part(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub(super) fn request_hash(input: &ReconcileSettlementAdjustmentInput) -> Result<String, AppError> {
    let canonical = serde_json::to_vec(input).map_err(|_| AppError::Internal)?;
    Ok(format!("{:x}", Sha256::digest(canonical)))
}
