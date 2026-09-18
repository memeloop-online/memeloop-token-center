use super::*;

// Only the one-time upgrade understands the retired storage format. Keep the
// exact historical AAD; changing it would make original credentials unreadable.
const LEGACY_AAD: &str = "memeloop-token-center/key-credential-recovery/v1";

#[derive(Deserialize)]
struct LegacyEnvelope {
    key_id: Uuid,
    credential_generation: i64,
    key: String,
}

fn invalid_upgrade() -> sqlx::Error {
    // Never include plaintext, ciphertext, hashes, or decoder errors here.
    sqlx::Error::Protocol("credential plaintext upgrade validation failed".into())
}

pub(super) async fn promote_active_plaintext(
    tx: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
    pepper: Option<&[u8]>,
) -> Result<(), sqlx::Error> {
    if matches!(backend, DatabaseBackend::PostgreSql) {
        // Fence old-version readers and writers for the whole promotion +
        // DROP transaction. A weaker lock lets an old copy reader hold an
        // envelope read lock while waiting to promote plaintext, deadlocking
        // this transaction when DROP upgrades to an exclusive lock.
        sqlx::query("LOCK TABLE key_records, key_credentials, key_credential_recovery_secrets IN ACCESS EXCLUSIVE MODE")
            .execute(&mut **tx)
            .await?;
    }
    let mut cursor = String::new();
    loop {
        let rows = sqlx::query(
            "SELECT c.id, c.key_id, c.generation, c.secret_hash, k.status, k.credential_generation, r.key_id AS envelope_key_id, r.credential_generation AS envelope_generation, r.ciphertext FROM key_credentials c JOIN key_records k ON k.id = c.key_id LEFT JOIN key_credential_recovery_secrets r ON r.credential_id = c.id WHERE c.revoked_at IS NULL AND c.generation = k.credential_generation AND k.status <> 'revoked' AND c.secret_plaintext IS NULL AND c.id > $1 ORDER BY c.id LIMIT 256",
        )
        .bind(&cursor)
        .fetch_all(&mut **tx)
        .await?;
        if rows.is_empty() {
            break;
        }
        let pepper = pepper.ok_or_else(invalid_upgrade)?;
        for row in rows {
            let id: String = row.try_get("id")?;
            let key_id: String = row.try_get("key_id")?;
            let key_uuid = Uuid::parse_str(&key_id).map_err(|_| invalid_upgrade())?;
            let generation: i64 = row.try_get("generation")?;
            let envelope_key_id: Option<String> = row.try_get("envelope_key_id")?;
            let envelope_generation: Option<i64> = row.try_get("envelope_generation")?;
            // Suspended credentials must not silently lose their only original
            // either. Require an explicit resolution before retiring storage.
            if row.try_get::<String, _>("status")? != "active"
                || row.try_get::<i64, _>("credential_generation")? != generation
                || envelope_key_id.as_deref() != Some(key_id.as_str())
                || envelope_generation != Some(generation)
            {
                return Err(invalid_upgrade());
            }
            let ciphertext: Option<String> = row.try_get("ciphertext")?;
            let ciphertext = ciphertext.ok_or_else(invalid_upgrade)?;
            let aad = format!("{LEGACY_AAD}/{key_uuid}/{generation}");
            let envelope: LegacyEnvelope = open_private_json(&ciphertext, pepper, aad.as_bytes())
                .map_err(|_| invalid_upgrade())?;
            let expected: Vec<u8> = row.try_get("secret_hash")?;
            if envelope.key_id != key_uuid
                || envelope.credential_generation != generation
                || !crypto::verify_credential(&envelope.key, pepper, &expected)
            {
                return Err(invalid_upgrade());
            }
            let updated = sqlx::query(
                "UPDATE key_credentials SET secret_plaintext = $1 WHERE id = $2 AND key_id = $3 AND generation = $4 AND revoked_at IS NULL AND secret_plaintext IS NULL AND EXISTS (SELECT 1 FROM key_records k WHERE k.id = key_credentials.key_id AND k.status = 'active' AND k.credential_generation = key_credentials.generation)",
            )
            .bind(&envelope.key)
            .bind(&id)
            .bind(&key_id)
            .bind(generation)
            .execute(&mut **tx)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(invalid_upgrade());
            }
            cursor = id;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEPPER: &[u8] = b"plaintext upgrade test pepper longer than thirty-two bytes";

    async fn issue(database: &Database, alias: &str) -> IssuedKey {
        database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "plaintext-upgrade".into(),
                    principal_external_id: alias.into(),
                    alias: alias.into(),
                    currency: "USD".into(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::ZERO,
                    idempotency_key: None,
                },
                PEPPER,
            )
            .await
            .unwrap()
    }

    async fn restore_legacy_tables(database: &Database) {
        sqlx::raw_sql(include_str!(
            "../../../migrations/common/0070_key_credential_recovery.sql"
        ))
        .execute(&database.pool)
        .await
        .unwrap();
        sqlx::raw_sql(include_str!(
            "../../../migrations/common/0079_key_credential_recovery_access_limits.sql"
        ))
        .execute(&database.pool)
        .await
        .unwrap();
        sqlx::query("DELETE FROM schema_migrations WHERE version = 110")
            .execute(&database.pool)
            .await
            .unwrap();
    }

    async fn envelope(database: &Database, issued: &IssuedKey, fault: &str) {
        let key_id = if fault == "payload_key" {
            Uuid::nil()
        } else {
            issued.key_id
        };
        let generation = if fault == "payload_generation" { 2 } else { 1 };
        let key = if fault == "hash" {
            "incorrect-original-value"
        } else {
            issued.key.as_str()
        };
        let aad = format!("{LEGACY_AAD}/{}/1", issued.key_id);
        let aad = if fault == "aad" {
            "incorrect-aad"
        } else {
            &aad
        };
        let ciphertext = seal_private_json(
            &serde_json::json!({ "key_id": key_id, "credential_generation": generation, "key": key }),
            PEPPER, aad.as_bytes(),
        ).unwrap();
        sqlx::query("INSERT INTO key_credential_recovery_secrets (credential_id, key_id, credential_generation, ciphertext, created_at, updated_at) SELECT id, key_id, $1, $2, 1, 1 FROM key_credentials WHERE key_id = $3 AND generation = 1")
            .bind(if fault == "row_generation" { 2_i64 } else { 1_i64 })
            .bind(ciphertext).bind(issued.key_id.to_string())
            .execute(&database.pool).await.unwrap();
        sqlx::query("UPDATE key_credentials SET secret_plaintext = NULL WHERE key_id = $1")
            .bind(issued.key_id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn upgrade_promotes_originals_before_drop_without_overwriting_or_rotating() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("upgrade.db").display()
        ))
        .await
        .unwrap();
        database.migrate().await.unwrap();
        let legacy = issue(&database, "legacy").await;
        let direct = issue(&database, "direct").await;
        restore_legacy_tables(&database).await;
        envelope(&database, &legacy, "valid").await;
        database
            .migrate_with_credential_pepper(PEPPER)
            .await
            .unwrap();
        database.migrate().await.unwrap();
        for issued in [&legacy, &direct] {
            let copied = database
                .copy_key_credential(issued.key_id, PEPPER, None, true)
                .await
                .unwrap();
            assert!(copied.key == issued.key);
            assert_eq!(copied.credential_generation, 1);
            database
                .authenticate_key(&copied.key, PEPPER)
                .await
                .unwrap();
        }
        let retired: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name LIKE 'key_credential_recovery_%'")
            .fetch_one(&database.pool).await.unwrap();
        assert_eq!(retired, 0);
    }

    #[tokio::test]
    async fn upgrade_failures_roll_back_all_promotions_and_preserve_original_envelopes() {
        for fault in [
            "missing_pepper",
            "wrong_pepper",
            "payload_key",
            "payload_generation",
            "row_generation",
            "hash",
            "aad",
            "missing_envelope",
            "suspended",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let database = Database::connect(&format!(
                "sqlite://{}?mode=rwc",
                directory.path().join("upgrade-failure.db").display()
            ))
            .await
            .unwrap();
            database.migrate().await.unwrap();
            let valid = issue(&database, "valid").await;
            let invalid = issue(&database, "invalid").await;
            restore_legacy_tables(&database).await;
            envelope(&database, &valid, "valid").await;
            envelope(&database, &invalid, fault).await;
            if fault == "missing_envelope" {
                sqlx::query("DELETE FROM key_credential_recovery_secrets WHERE key_id = $1")
                    .bind(invalid.key_id.to_string())
                    .execute(&database.pool)
                    .await
                    .unwrap();
            }
            if fault == "suspended" {
                database
                    .set_key_status(invalid.key_id, "suspended")
                    .await
                    .unwrap();
            }
            let result = match fault {
                "missing_pepper" => database.migrate().await,
                "wrong_pepper" => {
                    database
                        .migrate_with_credential_pepper(b"wrong pepper")
                        .await
                }
                _ => database.migrate_with_credential_pepper(PEPPER).await,
            };
            assert!(result.is_err(), "{fault}");
            let promoted: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM key_credentials WHERE secret_plaintext IS NOT NULL",
            )
            .fetch_one(&database.pool)
            .await
            .unwrap();
            assert_eq!(promoted, 0, "{fault}");
            let applied: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM schema_migrations WHERE version = 110")
                    .fetch_one(&database.pool)
                    .await
                    .unwrap();
            assert_eq!(applied, 0, "{fault}");
            let retained: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM key_credential_recovery_secrets")
                    .fetch_one(&database.pool)
                    .await
                    .unwrap();
            assert_eq!(
                retained,
                if fault == "missing_envelope" { 1 } else { 2 },
                "{fault}"
            );
        }
    }

    #[tokio::test]
    async fn already_copyable_upgrade_needs_no_pepper_and_never_replaces_plaintext() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("copyable-upgrade.db").display()
        ))
        .await
        .unwrap();
        database.migrate().await.unwrap();
        let issued = issue(&database, "already-copyable").await;
        restore_legacy_tables(&database).await;
        // An obsolete envelope must not overwrite an existing direct value.
        envelope(&database, &issued, "hash").await;
        sqlx::query("UPDATE key_credentials SET secret_plaintext = $1 WHERE key_id = $2")
            .bind(&issued.key)
            .bind(issued.key_id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        database.migrate().await.unwrap();
        let copied = database
            .copy_key_credential(issued.key_id, PEPPER, None, true)
            .await
            .unwrap();
        assert!(copied.key == issued.key);
    }

    #[tokio::test]
    async fn postgres_upgrade_locks_promotes_and_drops_only_after_complete_validation() {
        let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        sqlx::any::install_default_drivers();
        let pool = AnyPoolOptions::new()
            .max_connections(1)
            .connect(&database_url)
            .await
            .unwrap();
        for corrupt in [false, true] {
            let mut tx = pool.begin().await.unwrap();
            let schema = format!("credential_plaintext_v110_{}", Uuid::now_v7().simple());
            sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "SET LOCAL search_path = {schema}"
            )))
            .execute(&mut *tx)
            .await
            .unwrap();
            sqlx::raw_sql(
                "CREATE TABLE schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL);
                 CREATE TABLE key_records (id TEXT PRIMARY KEY, status TEXT NOT NULL, credential_generation BIGINT NOT NULL);
                 CREATE TABLE key_credentials (id TEXT PRIMARY KEY, key_id TEXT NOT NULL, generation BIGINT NOT NULL, secret_hash BYTEA NOT NULL, secret_plaintext TEXT, revoked_at BIGINT);",
            )
            .execute(&mut *tx)
            .await
            .unwrap();
            sqlx::raw_sql(include_str!(
                "../../../migrations/common/0070_key_credential_recovery.sql"
            ))
            .execute(&mut *tx)
            .await
            .unwrap();
            sqlx::raw_sql(include_str!(
                "../../../migrations/common/0079_key_credential_recovery_access_limits.sql"
            ))
            .execute(&mut *tx)
            .await
            .unwrap();
            // Fixed ordered IDs guarantee one valid promotion precedes the
            // deliberately corrupt second row in the rollback case.
            let mut originals = Vec::new();
            for index in [1_u128, 2] {
                let key_id = Uuid::from_u128(index);
                let issued = crypto::issue_credential(key_id, PEPPER);
                let id = key_id.to_string();
                sqlx::query("INSERT INTO key_records (id, status, credential_generation) VALUES ($1, 'active', 1)")
                    .bind(&id).execute(&mut *tx).await.unwrap();
                sqlx::query("INSERT INTO key_credentials (id, key_id, generation, secret_hash) VALUES ($1, $1, 1, $2)")
                    .bind(&id).bind(&issued.secret_hash).execute(&mut *tx).await.unwrap();
                let aad = format!("{LEGACY_AAD}/{key_id}/1");
                let payload_key = if corrupt && index == 2 {
                    "incorrect-original-value"
                } else {
                    issued.secret.as_str()
                };
                let ciphertext = seal_private_json(
                    &serde_json::json!({"key_id": key_id, "credential_generation": 1, "key": payload_key}),
                    PEPPER, aad.as_bytes(),
                ).unwrap();
                sqlx::query("INSERT INTO key_credential_recovery_secrets (credential_id, key_id, credential_generation, ciphertext, created_at, updated_at) VALUES ($1, $1, 1, $2, 1, 1)")
                    .bind(&id).bind(ciphertext).execute(&mut *tx).await.unwrap();
                originals.push(issued.secret);
            }
            sqlx::query("SAVEPOINT plaintext_upgrade")
                .execute(&mut *tx)
                .await
                .unwrap();
            let result =
                promote_active_plaintext(&mut tx, DatabaseBackend::PostgreSql, Some(PEPPER)).await;
            if corrupt {
                assert!(result.is_err());
                sqlx::query("ROLLBACK TO SAVEPOINT plaintext_upgrade")
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                let populated: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM key_credentials WHERE secret_plaintext IS NOT NULL",
                )
                .fetch_one(&mut *tx)
                .await
                .unwrap();
                assert_eq!(populated, 0);
                let envelopes: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM key_credential_recovery_secrets")
                        .fetch_one(&mut *tx)
                        .await
                        .unwrap();
                assert_eq!(envelopes, 2);
            } else {
                result.unwrap();
                let values: Vec<String> =
                    sqlx::query_scalar("SELECT secret_plaintext FROM key_credentials ORDER BY id")
                        .fetch_all(&mut *tx)
                        .await
                        .unwrap();
                assert!(values == originals);
                apply_migration_range(&mut tx, POSTGRES_MIGRATIONS, 110, 110)
                    .await
                    .unwrap();
                let retired: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = current_schema() AND table_name LIKE 'key_credential_recovery_%'")
                    .fetch_one(&mut *tx).await.unwrap();
                assert_eq!(retired, 0);
            }
            tx.rollback().await.unwrap();
        }
    }
}
