use super::super::*;

pub(super) async fn authenticate_legacy_key(
    database: &Database,
    value: &str,
    pepper: &[u8],
) -> Result<AuthenticatedKey, AppError> {
    if value.len() < 16 || value.len() > 512 || value.contains(['\r', '\n']) {
        return Err(AppError::Unauthorized);
    }
    let (secret_hash, _) = crypto::hash_credential(value, pepper);
    let row = sqlx::query(
        "SELECT k.id AS key_id, k.tenant_id, k.principal_id, k.account_id, k.alias, k.currency, k.policy_json, k.status, c.generation, c.secret_hash FROM key_records k JOIN legacy_key_credentials c ON c.key_id = k.id AND c.generation = k.credential_generation WHERE c.secret_hash = $1 AND c.revoked_at IS NULL",
    )
    .bind(secret_hash)
    .fetch_optional(&database.pool)
    .await?
    .ok_or(AppError::Unauthorized)?;
    super::authenticated_key_from_row(row, value, pepper)
}

impl Database {
    pub async fn register_legacy_key_credential(
        &self,
        key_id: Uuid,
        credential: &str,
        source_hash: &str,
        pepper: &[u8],
    ) -> Result<LegacyCredentialView, AppError> {
        let source_hash = source_hash.trim().to_ascii_lowercase();
        if credential.len() < 16
            || credential.len() > 512
            || credential.contains(['\r', '\n'])
            || source_hash.len() != 64
            || !source_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(AppError::BadRequest(
                "legacy credential or source_hash is invalid".into(),
            ));
        }
        let actual_source_hash = format!("{:x}", Sha256::digest(credential.trim().as_bytes()));
        if actual_source_hash != source_hash {
            return Err(AppError::BadRequest(
                "legacy credential does not match source_hash".into(),
            ));
        }
        // This flow reads the target and existing mappings before inserting.
        // On SQLite, a deferred transaction can lose the writer race after
        // those reads and fail its lock upgrade immediately with SQLITE_BUSY.
        // Claim the writer slot up front so the configured busy timeout can
        // serialize this migration write with request/background activity.
        let mut transaction = self.begin_write_transaction().await?;
        let target_sql = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT credential_generation, status FROM key_records WHERE id = $1 FOR UPDATE"
            }
            DatabaseBackend::Sqlite => {
                "SELECT credential_generation, status FROM key_records WHERE id = $1"
            }
        };
        let target = sqlx::query(target_sql)
            .bind(key_id.to_string())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(AppError::NotFound)?;
        if target.try_get::<String, _>("status")? != "active" {
            return Err(AppError::Conflict(
                "legacy credential target is not active".into(),
            ));
        }
        let existing = sqlx::query(
            "SELECT key_id, generation, fingerprint, revoked_at FROM legacy_key_credentials WHERE source_hash = $1",
        )
        .bind(&source_hash)
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(row) = existing {
            let existing_key_id = parse_uuid(row.try_get("key_id")?)?;
            if existing_key_id != key_id {
                return Err(AppError::Conflict(
                    "legacy credential source is already mapped to another key".into(),
                ));
            }
            if row.try_get::<Option<i64>, _>("revoked_at")?.is_some() {
                return Err(AppError::Conflict(
                    "legacy credential mapping has been revoked".into(),
                ));
            }
            transaction.commit().await?;
            return Ok(LegacyCredentialView {
                key_id,
                generation: row.try_get("generation")?,
                fingerprint: row.try_get("fingerprint")?,
                source_hash,
            });
        }
        if sqlx::query("SELECT id FROM legacy_key_credentials WHERE key_id = $1")
            .bind(key_id.to_string())
            .fetch_optional(&mut *transaction)
            .await?
            .is_some()
        {
            return Err(AppError::Conflict(
                "stable key already has a legacy credential mapping".into(),
            ));
        }
        let generation: i64 = target.try_get("credential_generation")?;
        let (secret_hash, fingerprint) = crypto::hash_credential(credential.trim(), pepper);
        let inserted = sqlx::query(
            "INSERT INTO legacy_key_credentials (id, key_id, generation, secret_hash, fingerprint, source_hash, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(key_id.to_string())
        .bind(generation)
        .bind(secret_hash)
        .bind(&fingerprint)
        .bind(&source_hash)
        .bind(unix_millis())
        .execute(&mut *transaction)
        .await;
        if let Err(error) = inserted {
            if error
                .as_database_error()
                .is_some_and(|database| database.is_unique_violation())
            {
                return Err(AppError::Conflict(
                    "legacy credential source or target is already mapped".into(),
                ));
            }
            return Err(AppError::from(error));
        }
        transaction.commit().await?;
        Ok(LegacyCredentialView {
            key_id,
            generation,
            fingerprint,
            source_hash,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::super::*;

    #[tokio::test]
    async fn legacy_credentials_are_one_to_one_and_do_not_duplicate_key_lists() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("legacy-one-to-one.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let pepper = b"legacy one to one pepper longer than thirty-two bytes";
        let mut keys = Vec::new();
        for principal in ["first", "second"] {
            keys.push(
                database
                    .create_key(
                        CreateKeyInput {
                            tenant_external_id: "legacy-one-to-one".to_owned(),
                            principal_external_id: principal.to_owned(),
                            alias: principal.to_owned(),
                            currency: "USD".to_owned(),
                            policy: KeyPolicy::default(),
                            initial_balance: Decimal::ONE,
                            idempotency_key: None,
                        },
                        pepper,
                    )
                    .await
                    .unwrap(),
            );
        }
        let first_secret = "legacy-one-to-one-credential-first";
        let first_hash = format!("{:x}", Sha256::digest(first_secret.as_bytes()));
        let attached = database
            .register_legacy_key_credential(keys[0].key_id, first_secret, &first_hash, pepper)
            .await
            .unwrap();
        assert_eq!(
            database
                .register_legacy_key_credential(keys[0].key_id, first_secret, &first_hash, pepper,)
                .await
                .unwrap()
                .source_hash,
            attached.source_hash
        );
        assert!(matches!(
            database
                .register_legacy_key_credential(keys[1].key_id, first_secret, &first_hash, pepper,)
                .await,
            Err(AppError::Conflict(_))
        ));
        let second_secret = "legacy-one-to-one-credential-second";
        let second_hash = format!("{:x}", Sha256::digest(second_secret.as_bytes()));
        assert!(matches!(
            database
                .register_legacy_key_credential(
                    keys[0].key_id,
                    second_secret,
                    &second_hash,
                    pepper,
                )
                .await,
            Err(AppError::Conflict(_))
        ));

        let managed = database
            .list_managed_keys(Some("legacy-one-to-one"), None)
            .await
            .unwrap();
        assert_eq!(managed.len(), 2);
        assert_eq!(
            managed
                .iter()
                .filter(|key| key.key_id == keys[0].key_id)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn sqlite_legacy_attachment_waits_for_a_competing_writer() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("legacy-writer-race.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let pepper = b"legacy writer race pepper longer than thirty-two bytes";
        let key = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "legacy-writer-race".to_owned(),
                    principal_external_id: "principal".to_owned(),
                    alias: "legacy".to_owned(),
                    currency: "USD".to_owned(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::ONE,
                    idempotency_key: None,
                },
                pepper,
            )
            .await
            .unwrap();

        let blocker = database.begin_write_transaction().await.unwrap();
        let secret = "legacy-writer-race-credential".to_owned();
        let source_hash = format!("{:x}", Sha256::digest(secret.as_bytes()));
        let attaching_database = database.clone();
        let attaching = tokio::spawn(async move {
            attaching_database
                .register_legacy_key_credential(key.key_id, &secret, &source_hash, pepper)
                .await
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !attaching.is_finished(),
            "legacy attachment must wait instead of failing a deferred lock upgrade"
        );
        blocker.rollback().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), attaching)
            .await
            .expect("legacy attachment did not resume after writer release")
            .expect("legacy attachment task panicked")
            .expect("legacy attachment failed after writer release");
    }
}
