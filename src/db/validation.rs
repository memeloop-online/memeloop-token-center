use rust_decimal::{Decimal, prelude::ToPrimitive};
use uuid::Uuid;

use super::GenerationJobIdempotency;
use crate::error::AppError;

pub(super) fn validate_currency(currency: &str) -> Result<(), AppError> {
    match currency.to_uppercase().as_str() {
        "USD" | "CNY" => Ok(()),
        _ => Err(AppError::BadRequest("currency must be USD or CNY".into())),
    }
}

pub(super) fn validate_idempotency_key(value: &str, field: &str) -> Result<(), AppError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 200 || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(AppError::BadRequest(format!(
            "{field} must contain at most 200 visible ASCII characters"
        )));
    }
    Ok(())
}

pub(super) fn validate_generation_job_idempotency(
    idempotency: &GenerationJobIdempotency,
) -> Result<(), AppError> {
    validate_idempotency_key(&idempotency.key, "Idempotency-Key")?;
    if idempotency.request_hash.len() != 64
        || !idempotency
            .request_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(AppError::BadRequest(
            "generation request hash must be a lowercase BLAKE3 hex digest".into(),
        ));
    }
    Ok(())
}

pub(super) fn decimal_to_micros(value: Decimal) -> Result<i64, AppError> {
    let scaled = value * Decimal::from(crate::model::MONEY_SCALE);
    if !scaled.fract().is_zero() {
        return Err(AppError::BadRequest(
            "monetary values support at most 6 decimal places".into(),
        ));
    }
    scaled
        .to_i64()
        .ok_or_else(|| AppError::BadRequest("monetary value is out of range".into()))
}

pub(super) fn validate_service_tier(service_tier: &str) -> Result<(), AppError> {
    if matches!(
        service_tier,
        "default" | "auto" | "priority" | "flex" | "scale" | "batch" | "standard_only"
    ) {
        Ok(())
    } else {
        Err(AppError::BadRequest(
            "service_tier must be default, auto, priority, flex, scale, batch, or standard_only"
                .into(),
        ))
    }
}

pub(super) fn parse_uuid(value: String) -> Result<Uuid, AppError> {
    Uuid::parse_str(&value).map_err(|_| AppError::Internal)
}

pub(super) fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
