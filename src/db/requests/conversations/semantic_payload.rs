use super::*;

/// Completed work needs an idempotency receipt, not another 1,024-hash copy
/// of the already materialized prefix. Pending snapshots remain unchanged.
pub(super) fn completed_receipt(snapshot: &str) -> String {
    let receipt = serde_json::json!({
        "completed_snapshot_digest_v1": blake3::hash(snapshot.as_bytes()).to_hex().to_string()
    })
    .to_string();
    if receipt.len() < snapshot.len() {
        receipt
    } else {
        snapshot.to_owned()
    }
}

pub(super) fn snapshot_matches(
    existing: Option<&str>,
    expected: Option<&str>,
    completed: bool,
) -> bool {
    if existing == expected {
        return true;
    }
    // New enqueues carry an empty placeholder; the deferred worker owns the
    // derived snapshot. Request identity/body/hints are checked by the caller,
    // so a replay must accept content the worker has already materialized.
    if expected
        .and_then(|encoded| serde_json::from_str::<ConversationSemanticSnapshot>(encoded).ok())
        .as_ref()
        .is_some_and(empty_semantic_snapshot)
    {
        return existing.is_some_and(|encoded| {
            serde_json::from_str::<ConversationSemanticSnapshot>(encoded).is_ok()
                || (completed
                    && serde_json::from_str::<serde_json::Value>(encoded)
                        .ok()
                        .is_some_and(|value| {
                            value["completed_snapshot_digest_v1"]
                                .as_str()
                                .is_some_and(|digest| {
                                    digest.len() == 64
                                        && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                                })
                        }))
        });
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

impl Database {
    /// Keyset-walk every outbox row, not an unindexed filter for completed
    /// snapshots. At most 32 rows and 128 KiB per snapshot leave the database.
    /// Pending jobs, malformed snapshots and missing retained paths are kept.
    /// A full pass can restart later to catch jobs that completed behind it.
    pub(crate) async fn compact_completed_conversation_receipts(
        &self,
        after: Option<&str>,
    ) -> Result<(Option<String>, u64), AppError> {
        let rows = sqlx::query(
            "SELECT request_id, tenant_id, key_id, \
             CASE WHEN projected_at IS NOT NULL \
                  AND LENGTH(COALESCE(semantic_payload_snapshot_json, semantic_snapshot_json)) BETWEEN 128 AND 131072 \
                  THEN COALESCE(semantic_payload_snapshot_json, semantic_snapshot_json) \
                  ELSE NULL END AS snapshot \
             FROM conversation_projection_outbox WHERE request_id > $1 \
             ORDER BY request_id LIMIT 32",
        )
        .bind(after.unwrap_or(""))
        .fetch_all(&self.pool)
        .await?;
        let mut last = None;
        let mut compacted = 0;
        for row in rows {
            let request: String = row.try_get("request_id")?;
            last = Some(request.clone());
            let Some(encoded) = row.try_get::<Option<String>, _>("snapshot")? else {
                continue;
            };
            let Ok(snapshot) = serde_json::from_str::<ConversationSemanticSnapshot>(&encoded)
            else {
                continue;
            };
            let receipt = completed_receipt(&encoded);
            if receipt.len() >= encoded.len() {
                continue;
            }
            let tenant: String = row.try_get("tenant_id")?;
            let key: String = row.try_get("key_id")?;
            // Compare-and-swap protects concurrent projection/replay and HA
            // maintenance. Require the exact owned observation/leaf before
            // replacing its recovery evidence; do not infer success from a
            // timestamp alone.
            let mut tx = self.begin_write_transaction().await?;
            let updated = sqlx::query(
                "UPDATE conversation_projection_outbox \
                 SET semantic_snapshot_json = CASE WHEN semantic_payload_snapshot_json IS NULL THEN $1 ELSE NULL END, \
                     semantic_payload_snapshot_json = CASE WHEN semantic_payload_snapshot_json IS NOT NULL THEN $1 ELSE NULL END \
                 WHERE request_id = $2 AND tenant_id = $3 AND key_id = $4 \
                   AND projected_at IS NOT NULL \
                   AND COALESCE(semantic_payload_snapshot_json, semantic_snapshot_json) = $5 \
                   AND EXISTS (SELECT 1 FROM conversation_observations o \
                       JOIN conversation_clusters c ON c.id = o.cluster_id \
                       WHERE o.request_id = $2 AND o.key_id = $4 AND c.tenant_id = $3 \
                         AND (o.leaf_node_hash = $6 OR \
                              (o.leaf_node_hash IS NULL AND CAST($6 AS TEXT) IS NULL)))",
            )
            .bind(&receipt)
            .bind(&request)
            .bind(&tenant)
            .bind(&key)
            .bind(&encoded)
            .bind(snapshot.leaf.as_deref())
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() == 1 {
                // Old enqueue implementations could recreate this consumed
                // payload on a completed replay. It is no longer recovery
                // input once the matching retained path has been verified.
                sqlx::query("DELETE FROM conversation_semantic_payloads WHERE request_id = $1 AND tenant_id = $2 AND key_id = $3")
                    .bind(&request).bind(&tenant).bind(&key).execute(&mut *tx).await?;
                compacted += 1;
            }
            tx.commit().await?;
        }
        Ok((last, compacted))
    }
}

/// 快照还是空哨兵时，用这行缩减后的对话重试。不把它当成会话正文。
pub(super) async fn load_reduced(
    database: &Database,
    request_id: Uuid,
    tenant_id: &str,
    key_id: &str,
    principal_id: &str,
) -> Result<Option<String>, AppError> {
    let length = match database.backend {
        DatabaseBackend::PostgreSql => "octet_length(request_json)",
        DatabaseBackend::Sqlite => "length(CAST(request_json AS BLOB))",
    };
    let statement = format!(
        "SELECT tenant_id, key_id, principal_id, format_version, encoded_bytes, digest, request_json FROM conversation_semantic_payloads WHERE request_id = $1 AND encoded_bytes BETWEEN 2 AND 134217728 AND {length} = encoded_bytes"
    );
    let Some(row) = sqlx::query(sqlx::AssertSqlSafe(statement))
        .bind(request_id.to_string())
        .fetch_optional(&database.pool)
        .await?
    else {
        return Ok(None);
    };
    let expected: String = row.try_get("digest")?;
    Ok(Some(validated_body(
        &row,
        request_id,
        tenant_id,
        key_id,
        principal_id,
        &expected,
    )?))
}

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

fn validated_body(
    row: &sqlx::any::AnyRow,
    request_id: Uuid,
    tenant_id: &str,
    key_id: &str,
    principal_id: &str,
    expected: &str,
) -> Result<String, AppError> {
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
    if !json_document(&body) {
        return Err(AppError::Conflict(
            "invalid conversation semantic payload".into(),
        ));
    }
    Ok(body)
}

fn json_document(body: &str) -> bool {
    let mut deserializer = serde_json::Deserializer::from_str(body);
    serde::de::IgnoredAny::deserialize(&mut deserializer).is_ok() && deserializer.end().is_ok()
}

pub(super) async fn load(
    database: &Database,
    request_id: Uuid,
    tenant_id: &str,
    key_id: &str,
    principal_id: &str,
    expected: &str,
) -> Result<String, AppError> {
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
    validated_body(&row, request_id, tenant_id, key_id, principal_id, expected)
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
    validated_body(
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
        let empty = r#"{"leaf":null,"atom_hashes":[]}"#;
        assert!(snapshot_matches(Some(&snapshot), Some(empty), false));
        assert!(snapshot_matches(Some(&receipt), Some(empty), true));
        assert!(!snapshot_matches(Some(&receipt), Some(empty), false));
        assert!(!snapshot_matches(Some("{}"), Some(empty), true));
        assert!(!snapshot_matches(
            Some(r#"{"completed_snapshot_digest_v1":"invalid"}"#),
            Some(empty),
            true
        ));
        assert_eq!(
            completed_receipt(empty),
            empty,
            "small snapshots never grow"
        );
    }

    #[tokio::test]
    async fn historical_compaction_is_bounded_and_requires_completed_retained_evidence() {
        let directory = tempfile::tempdir().unwrap();
        historical_compaction_contract(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("receipt-history.db").display()
        ))
        .await;
    }

    #[tokio::test]
    async fn postgres_historical_compaction_preserves_pending_and_missing_history() {
        let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        let admin = sqlx::PgPool::connect(&url).await.unwrap();
        let schema = format!("receipt_compaction_{}", Uuid::now_v7().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let mut isolated = url::Url::parse(&url).unwrap();
        isolated
            .query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        historical_compaction_contract(isolated.as_str()).await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
    }

    async fn historical_compaction_contract(url: &str) {
        use super::super::prefix_storage::tests::{key, observe};
        let db = Database::connect(url).await.unwrap();
        db.migrate().await.unwrap();
        let key = key(&db, "receipt-history").await;
        let body = serde_json::json!({"input":[{"role":"user","content":"retained question"}]});
        let hints = ConversationHints::default();
        for index in 1..=35 {
            let mut tx = db.begin_write_transaction().await.unwrap();
            enqueue_conversation_projection_in_transaction(
                &mut tx,
                ConversationProjectionEnqueueInput {
                    request_id: Uuid::from_u128(index),
                    key: &key,
                    request_json: &body,
                    hints: &hints,
                    client_name: Some("Codex"),
                    upstream_response_id: None,
                    observed_at: index as i64,
                },
            )
            .await
            .unwrap();
            // Seed the legacy recovery rows explicitly: current enqueue keeps
            // bounded inline JSON and no longer duplicates it in this table.
            let encoded = body.to_string();
            let request_id = Uuid::from_u128(index).to_string();
            let payload_digest = digest(
                &request_id,
                &key.tenant_id.to_string(),
                &key.key_id.to_string(),
                &key.principal_id.to_string(),
                &encoded,
            );
            sqlx::query("INSERT INTO conversation_semantic_payloads (request_id, tenant_id, key_id, principal_id, format_version, encoded_bytes, digest, request_json) VALUES ($1, $2, $3, $4, 1, $5, $6, $7)")
                .bind(request_id).bind(key.tenant_id.to_string()).bind(key.key_id.to_string())
                .bind(key.principal_id.to_string()).bind(encoded.len() as i64)
                .bind(payload_digest).bind(encoded).execute(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
        }
        observe(&db, &key, &body, &hints, Uuid::from_u128(1), 1, true).await;
        observe(&db, &key, &body, &hints, Uuid::from_u128(2), 2, true).await;
        observe(
            &db,
            &key,
            &serde_json::json!({"input":[{"role":"user","content":"different retained path"}]}),
            &hints,
            Uuid::from_u128(4),
            4,
            true,
        )
        .await;
        let atoms = crate::conversation::extract_atoms(&body);
        let nodes = crate::conversation::build_prefix(&atoms);
        let planted = serde_json::to_string(&ConversationSemanticSnapshot {
            leaf: nodes.last().map(|node| node.node_hash.clone()),
            atom_hashes: atoms.iter().map(|atom| atom.content_hash.clone()).collect(),
            payload_digest: None,
        })
        .unwrap();
        sqlx::query(
            "UPDATE conversation_projection_outbox SET semantic_snapshot_json = $1, semantic_payload_snapshot_json = NULL",
        )
        .bind(&planted)
        .execute(&db.pool)
        .await
        .unwrap();
        let encoded: String = sqlx::query_scalar(
            "SELECT semantic_snapshot_json FROM conversation_projection_outbox WHERE request_id = $1",
        )
        .bind(Uuid::from_u128(1).to_string())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        // One real completion, one missing path, and one mismatched path.
        // Pending rows keep recovery evidence even with a matching path.
        sqlx::query(
            "UPDATE conversation_projection_outbox SET projected_at = 100 WHERE request_id IN ($1, $2, $3)",
        )
        .bind(Uuid::from_u128(1).to_string())
        .bind(Uuid::from_u128(3).to_string())
        .bind(Uuid::from_u128(4).to_string())
        .execute(&db.pool)
        .await
        .unwrap();
        let (next, compacted) = db
            .compact_completed_conversation_receipts(None)
            .await
            .unwrap();
        assert_eq!(next, Some(Uuid::from_u128(32).to_string()));
        assert_eq!(compacted, 1);
        let receipt: String = sqlx::query_scalar(
            "SELECT semantic_snapshot_json FROM conversation_projection_outbox WHERE request_id = $1",
        )
        .bind(Uuid::from_u128(1).to_string())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(snapshot_matches(Some(&receipt), Some(&encoded), true));
        assert!(receipt.len() < encoded.len());
        let compacted_payload: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_semantic_payloads WHERE request_id = $1",
        )
        .bind(Uuid::from_u128(1).to_string())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            compacted_payload, 0,
            "completed receipt drops its reduced dialogue"
        );
        let payloads: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM conversation_semantic_payloads")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            payloads, 34,
            "pending rows keep the reduced dialogue for retry"
        );
        let unchanged: String = sqlx::query_scalar(
            "SELECT semantic_snapshot_json FROM conversation_projection_outbox WHERE request_id = $1",
        )
        .bind(Uuid::from_u128(3).to_string())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(serde_json::from_str::<ConversationSemanticSnapshot>(&unchanged).is_ok());
        let (next, compacted) = db
            .compact_completed_conversation_receipts(next.as_deref())
            .await
            .unwrap();
        assert_eq!(next, Some(Uuid::from_u128(35).to_string()));
        assert_eq!(compacted, 0);
        assert_eq!(
            db.compact_completed_conversation_receipts(next.as_deref())
                .await
                .unwrap(),
            (None, 0)
        );
        assert_eq!(
            db.compact_completed_conversation_receipts(None)
                .await
                .unwrap()
                .1,
            0,
            "another sweep must not rehash a completed receipt"
        );
        db.pool.close().await;
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
        let hints = ConversationHints::default();
        let empty = serde_json::json!({});
        let enqueue = |client_name| ConversationProjectionEnqueueInput {
            request_id: request,
            key: &key,
            request_json: &empty,
            hints: &hints,
            client_name,
            upstream_response_id: None,
            observed_at: 1,
        };
        let mut tx = db.begin_write_transaction().await.unwrap();
        enqueue_conversation_projection_in_transaction(&mut tx, enqueue(Some("Codex")))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let snapshot: String = sqlx::query_scalar("SELECT COALESCE(semantic_payload_snapshot_json, semantic_snapshot_json) FROM conversation_projection_outbox WHERE request_id = $1")
            .bind(request.to_string()).fetch_one(&db.pool).await.unwrap();
        sqlx::query("UPDATE conversation_projection_outbox SET projected_at = 2, semantic_snapshot_json = $1, semantic_payload_snapshot_json = NULL WHERE request_id = $2")
            .bind(completed_receipt(&snapshot)).bind(request.to_string()).execute(&db.pool).await.unwrap();
        sqlx::query("DELETE FROM conversation_semantic_payloads WHERE request_id = $1")
            .bind(request.to_string())
            .execute(&db.pool)
            .await
            .unwrap();
        let mut tx = db.begin_write_transaction().await.unwrap();
        enqueue_conversation_projection_in_transaction(&mut tx, enqueue(Some("Codex")))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_semantic_payloads")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        let mut tx = db.begin_write_transaction().await.unwrap();
        assert!(
            enqueue_conversation_projection_in_transaction(&mut tx, enqueue(Some("other")))
                .await
                .is_err()
        );
    }
}
