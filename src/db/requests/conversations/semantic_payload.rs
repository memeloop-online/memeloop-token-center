use super::*;

/// Completed work needs an idempotency receipt, not another 1,024-hash copy
/// of the already materialized prefix. Pending snapshots remain unchanged.
pub(super) fn completed_receipt(snapshot: &str) -> String {
    serde_json::json!({
        "completed_snapshot_digest_v1": blake3::hash(snapshot.as_bytes()).to_hex().to_string()
    })
    .to_string()
}

pub(super) fn snapshot_matches(
    existing: Option<&str>,
    expected: Option<&str>,
    completed: bool,
) -> bool {
    if existing == expected {
        return true;
    }
    if !completed {
        return false;
    }
    match (existing, expected) {
        (Some(existing), Some(expected)) => existing == completed_receipt(expected),
        _ => false,
    }
}

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

#[cfg(test)]
mod receipt_tests {
    use super::*;

    #[test]
    fn receipts_are_small_and_accept_only_matching_completed_snapshots() {
        let snapshot =
            serde_json::json!({"leaf":"node","atom_hashes":vec!["a".repeat(64);1024]}).to_string();
        let receipt = completed_receipt(&snapshot);
        assert!(receipt.len() < 128);
        assert!(snapshot_matches(Some(&receipt), Some(&snapshot), true));
        assert!(!snapshot_matches(Some(&receipt), Some(&snapshot), false));
        assert!(!snapshot_matches(Some(&receipt), Some("changed"), true));
        assert!(
            snapshot_matches(Some(&snapshot), Some(&snapshot), true),
            "legacy completed rows remain replayable"
        );
    }

    #[tokio::test]
    async fn completed_enqueue_replay_never_recreates_consumed_payload() {
        use super::super::prefix_storage::tests::key;
        let directory = tempfile::tempdir().unwrap();
        let db = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("receipt.db").display()
        ))
        .await
        .unwrap();
        db.migrate().await.unwrap();
        let key = key(&db, "receipt").await;
        let request = Uuid::now_v7();
        let body = serde_json::json!({"input":[{"role":"user","content":"retained question"}]});
        let hints = ConversationHints::default();
        let enqueue = |request_json| ConversationProjectionEnqueueInput {
            request_id: request,
            key: &key,
            request_json,
            hints: &hints,
            client_name: Some("Codex"),
            upstream_response_id: None,
            observed_at: 1,
            content_materialized: false,
        };
        let mut tx = db.begin_write_transaction().await.unwrap();
        enqueue_conversation_projection_in_transaction(&mut tx, enqueue(&body))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let snapshot: String = sqlx::query_scalar("SELECT semantic_payload_snapshot_json FROM conversation_projection_outbox WHERE request_id = $1")
            .bind(request.to_string()).fetch_one(&db.pool).await.unwrap();
        sqlx::query("UPDATE conversation_projection_outbox SET projected_at = 2, semantic_payload_snapshot_json = $1 WHERE request_id = $2")
            .bind(completed_receipt(&snapshot)).bind(request.to_string()).execute(&db.pool).await.unwrap();
        sqlx::query("DELETE FROM conversation_semantic_payloads WHERE request_id = $1")
            .bind(request.to_string())
            .execute(&db.pool)
            .await
            .unwrap();
        let mut tx = db.begin_write_transaction().await.unwrap();
        enqueue_conversation_projection_in_transaction(&mut tx, enqueue(&body))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_semantic_payloads")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        let changed = serde_json::json!({"input":"different evidence"});
        let mut tx = db.begin_write_transaction().await.unwrap();
        assert!(
            enqueue_conversation_projection_in_transaction(&mut tx, enqueue(&changed))
                .await
                .is_err()
        );
    }
}
