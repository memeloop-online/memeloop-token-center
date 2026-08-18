use super::*;

mod keys;
mod legacy;
mod service_tokens;

pub use keys::CreateKeyInput;
pub use service_tokens::CreateServiceTokenInput;

fn authenticated_key_from_row(
    row: AnyRow,
    value: &str,
    pepper: &[u8],
) -> Result<AuthenticatedKey, AppError> {
    let status: String = row.try_get("status")?;
    let expected: Vec<u8> = row.try_get("secret_hash")?;
    if status != "active" || !crypto::verify_credential(value, pepper, &expected) {
        return Err(AppError::Unauthorized);
    }

    let policy_json: String = row.try_get("policy_json")?;
    Ok(AuthenticatedKey {
        key_id: parse_uuid(row.try_get("key_id")?)?,
        tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
        principal_id: parse_uuid(row.try_get("principal_id")?)?,
        account_id: parse_uuid(row.try_get("account_id")?)?,
        alias: row.try_get("alias")?,
        currency: row.try_get("currency")?,
        credential_generation: row.try_get("generation")?,
        policy: serde_json::from_str(&policy_json).map_err(|_| AppError::Internal)?,
    })
}
