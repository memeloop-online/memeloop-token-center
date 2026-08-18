use super::super::*;

pub(super) fn parse_decimal(value: &str, field: &str) -> Result<Decimal, AppError> {
    Decimal::from_str(value)
        .map_err(|_| AppError::BadRequest(format!("{field} must be a decimal string")))
}

pub(in crate::api) fn parse_money_micros(value: &str, field: &str) -> Result<i64, AppError> {
    let decimal = parse_decimal(value, field)?;
    if decimal.is_sign_negative() {
        return Err(AppError::BadRequest(format!(
            "{field} must not be negative"
        )));
    }
    decimal
        .checked_mul(Decimal::from(crate::model::MONEY_SCALE))
        .filter(|scaled| scaled.fract().is_zero())
        .and_then(|scaled| scaled.to_string().parse::<i64>().ok())
        .ok_or_else(|| {
            AppError::BadRequest(format!(
                "{field} must have at most 6 decimal places and fit monetary range"
            ))
        })
}
