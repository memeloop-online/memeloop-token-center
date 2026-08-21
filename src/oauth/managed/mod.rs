pub mod codex;
pub mod legacy_gemini;

use chrono::DateTime;

use crate::error::AppError;

const MAX_TOKEN_BYTES: usize = 128 * 1024;
const MAX_ACCOUNT_ID_BYTES: usize = 512;
const MAX_ACCOUNT_NAME_BYTES: usize = 200;
const MAX_PROJECT_ID_BYTES: usize = 256;

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

fn project_id(value: &str, kind: &str) -> Result<(), AppError> {
    controlled_text(value, MAX_PROJECT_ID_BYTES, false, kind)
}

pub(super) fn account_name(
    value: Option<&str>,
    fallback: &str,
    kind: &str,
) -> Result<String, AppError> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(fallback.to_owned());
    };
    controlled_text(value, MAX_ACCOUNT_NAME_BYTES, false, kind)?;
    Ok(value.to_owned())
}

fn controlled_text(
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

fn timestamp_millis(value: &str, kind: &str) -> Result<i64, AppError> {
    controlled_text(value, 64, false, kind)?;
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.timestamp_millis())
        .map_err(|_| invalid_document(kind))
}
