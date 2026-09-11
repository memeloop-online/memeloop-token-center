pub mod codex;
pub mod kimi;

use crate::error::AppError;

const MAX_TOKEN_BYTES: usize = 128 * 1024;
const MAX_ACCOUNT_ID_BYTES: usize = 512;

fn invalid_document(kind: &str) -> AppError {
    AppError::BadRequest(format!("{kind} OAuth document is invalid"))
}

fn required_secret(value: &str, kind: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > MAX_TOKEN_BYTES
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(invalid_document(kind));
    }
    Ok(())
}

fn bearer_token(value: &str, kind: &str) -> Result<(), AppError> {
    required_secret(value, kind)?;
    reqwest::header::HeaderValue::from_str(&format!("Bearer {value}"))
        .map(|_| ())
        .map_err(|_| invalid_document(kind))
}

fn optional_secret(value: Option<&str>, kind: &str) -> Result<(), AppError> {
    if let Some(value) = value {
        required_secret(value, kind)?;
    }
    Ok(())
}

pub(super) fn account_id(value: &str, kind: &str) -> Result<(), AppError> {
    controlled_text(value, MAX_ACCOUNT_ID_BYTES, false, kind)
}

pub(super) fn controlled_text(
    value: &str,
    max_bytes: usize,
    allow_empty: bool,
    kind: &str,
) -> Result<(), AppError> {
    if (!allow_empty && value.is_empty())
        || value.len() > max_bytes
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(invalid_document(kind));
    }
    Ok(())
}
