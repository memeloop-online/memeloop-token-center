use super::*;

const MAX_SEMANTIC_PAYLOAD_BYTES: usize = 128 * 1024 * 1024;

fn digest(
    request_id: &str,
    tenant_id: &str,
    key_id: &str,
    principal_id: &str,
    body: &str,
) -> String {
    let mut digest = blake3::Hasher::new_derive_key("MTC conversation semantic payload v1");
    for value in [request_id, tenant_id, key_id, principal_id, body] {
        digest.update(&(value.len() as u64).to_le_bytes());
        digest.update(value.as_bytes());
    }
    digest.finalize().to_hex().to_string()
}

pub(super) fn encode(
    request_id: Uuid,
    key: &AuthenticatedKey,
    request: &serde_json::Value,
) -> Result<(String, String), AppError> {
    let length = crate::gateway_body::memory::json_encoded_length(request)?;
    if length > MAX_SEMANTIC_PAYLOAD_BYTES {
        return Err(AppError::Conflict(
            "conversation semantic payload exceeds its bound".into(),
        ));
    }
    let mut bytes = Vec::with_capacity(length);
    serde_json::to_writer(&mut bytes, request).map_err(|_| AppError::Internal)?;
    let body = String::from_utf8(bytes).map_err(|_| AppError::Internal)?;
    let digest = digest(
        &request_id.to_string(),
        &key.tenant_id.to_string(),
        &key.key_id.to_string(),
        &key.principal_id.to_string(),
        &body,
    );
    Ok((body, digest))
}

pub(super) async fn persist(
    transaction: &mut Transaction<'_, Any>,
    request_id: Uuid,
    key: &AuthenticatedKey,
    body: &str,
    digest: &str,
) -> Result<(), AppError> {
    let inserted = sqlx::query("INSERT INTO conversation_semantic_payloads (request_id, tenant_id, key_id, principal_id, format_version, encoded_bytes, digest, request_json) VALUES ($1, $2, $3, $4, 1, $5, $6, $7) ON CONFLICT(request_id) DO NOTHING")
        .bind(request_id.to_string()).bind(key.tenant_id.to_string()).bind(key.key_id.to_string()).bind(key.principal_id.to_string()).bind(body.len() as i64).bind(digest).bind(body).execute(&mut **transaction).await?;
    if inserted.rows_affected() != 1 {
        let existing = sqlx::query("SELECT tenant_id, key_id, principal_id, format_version, encoded_bytes, digest, request_json FROM conversation_semantic_payloads WHERE request_id = $1")
            .bind(request_id.to_string()).fetch_one(&mut **transaction).await?;
        let decoded = validate(
            &existing,
            request_id,
            &key.tenant_id.to_string(),
            &key.key_id.to_string(),
            &key.principal_id.to_string(),
            digest,
        )?;
        if serde_json::to_string(&decoded).map_err(|_| AppError::Internal)? != body {
            return Err(AppError::Conflict(
                "conversation semantic payload changed".into(),
            ));
        }
    }
    Ok(())
}

fn validate(
    row: &sqlx::any::AnyRow,
    request_id: Uuid,
    tenant_id: &str,
    key_id: &str,
    principal_id: &str,
    expected: &str,
) -> Result<serde_json::Value, AppError> {
    let body: String = row.try_get("request_json")?;
    if row.try_get::<String, _>("tenant_id")? != tenant_id
        || row.try_get::<String, _>("key_id")? != key_id
        || row.try_get::<String, _>("principal_id")? != principal_id
        || row.try_get::<i64, _>("format_version")? != 1
        || body.len() > MAX_SEMANTIC_PAYLOAD_BYTES
        || row.try_get::<i64, _>("encoded_bytes")? != body.len() as i64
        || row.try_get::<String, _>("digest")? != expected
        || digest(
            &request_id.to_string(),
            tenant_id,
            key_id,
            principal_id,
            &body,
        ) != expected
    {
        return Err(AppError::Conflict(
            "conversation semantic payload integrity mismatch".into(),
        ));
    }
    serde_json::from_str(&body)
        .map_err(|_| AppError::Conflict("invalid conversation semantic payload".into()))
}

pub(super) async fn load(
    database: &Database,
    request_id: Uuid,
    tenant_id: &str,
    key_id: &str,
    principal_id: &str,
    expected: &str,
) -> Result<serde_json::Value, AppError> {
    let length = match database.backend {
        DatabaseBackend::PostgreSql => "octet_length(request_json)",
        DatabaseBackend::Sqlite => "length(CAST(request_json AS BLOB))",
    };
    let statement = format!(
        "SELECT tenant_id, key_id, principal_id, format_version, encoded_bytes, digest, request_json FROM conversation_semantic_payloads WHERE request_id = $1 AND encoded_bytes BETWEEN 2 AND 134217728 AND {length} = encoded_bytes"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(statement))
        .bind(request_id.to_string())
        .fetch_optional(&database.pool)
        .await?
        .ok_or_else(|| AppError::Conflict("conversation semantic payload is missing".into()))?;
    validate(&row, request_id, tenant_id, key_id, principal_id, expected)
}

pub(super) async fn verify_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
    request_id: Uuid,
    key: &AuthenticatedKey,
    expected: &str,
) -> Result<(), AppError> {
    let (length, lock) = match backend {
        DatabaseBackend::PostgreSql => ("octet_length(request_json)", " FOR UPDATE"),
        DatabaseBackend::Sqlite => ("length(CAST(request_json AS BLOB))", ""),
    };
    let statement = format!(
        "SELECT tenant_id, key_id, principal_id, format_version, encoded_bytes, digest, request_json FROM conversation_semantic_payloads WHERE request_id = $1 AND encoded_bytes BETWEEN 2 AND 134217728 AND {length} = encoded_bytes{lock}"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(statement))
        .bind(request_id.to_string())
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or_else(|| AppError::Conflict("conversation semantic payload is missing".into()))?;
    validate(
        &row,
        request_id,
        &key.tenant_id.to_string(),
        &key.key_id.to_string(),
        &key.principal_id.to_string(),
        expected,
    )?;
    Ok(())
}
