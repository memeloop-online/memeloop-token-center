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
        // Multiplication preserves the input scale in `rust_decimal`, so an
        // exact six-place amount can stringify as `5529585.000000` even
        // though it is an integer number of micros. Normalize before parsing
        // or valid settlement corrections at the maximum supported precision
        // are rejected.
        .and_then(|scaled| scaled.normalize().to_string().parse::<i64>().ok())
        .ok_or_else(|| {
            AppError::BadRequest(format!(
                "{field} must have at most 6 decimal places and fit monetary range"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::parse_money_micros;

    #[test]
    fn accepts_exact_maximum_money_precision() {
        assert_eq!(parse_money_micros("5.529585", "amount").unwrap(), 5_529_585);
    }
}
