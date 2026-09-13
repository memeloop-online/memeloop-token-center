use super::super::*;

const KEY_CREDENTIAL_RECOVERY_AAD_PREFIX: &str = "memeloop-token-center/key-credential-recovery/v1";

#[derive(Deserialize, Serialize)]
struct KeyCredentialRecoveryEnvelope {
    key_id: Uuid,
    credential_generation: i64,
    key: String,
}

struct KeyCredentialRecoverySecret<'a> {
    credential_id: Uuid,
    key_id: Uuid,
    generation: i64,
    credential: &'a str,
}

impl Database {
    /// Stores a caller-supplied original credential only when it exactly
    /// matches the active stable key and generation. This supports authorized
    /// imports without changing the key, generation, or authentication hash.
    pub async fn store_key_credential_recovery_secret(
        &self,
        key_id: Uuid,
        credential: &str,
        pepper: &[u8],
        _actor_service_id: Option<Uuid>,
    ) -> Result<(), AppError> {
        if credential.len() < 16
            || credential.len() > 512
            || credential.contains(['\0', '\r', '\n'])
        {
            return Err(AppError::BadRequest("credential is invalid".into()));
        }
        let mut tx = self.begin_write_transaction().await?;
        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT k.status, k.credential_generation, c.id AS credential_id, c.secret_hash FROM key_records k JOIN key_credentials c ON c.key_id = k.id AND c.generation = k.credential_generation AND c.revoked_at IS NULL WHERE k.id = $1 FOR UPDATE OF k"
            }
            DatabaseBackend::Sqlite => {
                "SELECT k.status, k.credential_generation, c.id AS credential_id, c.secret_hash FROM key_records k JOIN key_credentials c ON c.key_id = k.id AND c.generation = k.credential_generation AND c.revoked_at IS NULL WHERE k.id = $1"
            }
        };
        let current = sqlx::query(select)
            .bind(key_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
        if current.try_get::<String, _>("status")? != "active" {
            return Err(AppError::Forbidden);
        }
        let expected: Vec<u8> = current.try_get("secret_hash")?;
        if !crypto::verify_credential(credential, pepper, &expected) {
            return Err(AppError::BadRequest(
                "credential does not match the active key".into(),
            ));
        }
        // The caller supplied the active original credential, so make it
        // directly copyable. Historical hash-only values are never guessed.
        sqlx::query("UPDATE key_credentials SET secret_plaintext = $1 WHERE id = $2")
            .bind(credential)
            .bind(current.try_get::<String, _>("credential_id")?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Returns plaintext only through an explicit management action. Direct
    /// storage is rechecked against the active hash; legacy envelopes are also
    /// verified before being promoted to direct storage.
    pub async fn copy_key_credential(
        &self,
        key_id: Uuid,
        pepper: &[u8],
        actor_service_id: Option<Uuid>,
        actor_tenant_external_id: Option<&str>,
        actor_allows_recovery: bool,
    ) -> Result<RecoveredClientCredential, AppError> {
        let mut tx = self.begin_write_transaction().await?;
        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT k.tenant_id, t.external_id AS tenant_external_id, k.status, k.credential_generation, c.id AS credential_id, c.secret_hash, c.secret_plaintext, r.ciphertext FROM key_records k JOIN tenants t ON t.id = k.tenant_id LEFT JOIN key_credentials c ON c.key_id = k.id AND c.generation = k.credential_generation AND c.revoked_at IS NULL LEFT JOIN key_credential_recovery_secrets r ON r.credential_id = c.id AND r.key_id = k.id AND r.credential_generation = k.credential_generation WHERE k.id = $1 FOR UPDATE OF k"
            }
            DatabaseBackend::Sqlite => {
                "SELECT k.tenant_id, t.external_id AS tenant_external_id, k.status, k.credential_generation, c.id AS credential_id, c.secret_hash, c.secret_plaintext, r.ciphertext FROM key_records k JOIN tenants t ON t.id = k.tenant_id LEFT JOIN key_credentials c ON c.key_id = k.id AND c.generation = k.credential_generation AND c.revoked_at IS NULL LEFT JOIN key_credential_recovery_secrets r ON r.credential_id = c.id AND r.key_id = k.id AND r.credential_generation = k.credential_generation WHERE k.id = $1"
            }
        };
        let current = sqlx::query(select)
            .bind(key_id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        let Some(current) = current else {
            // A tenant-bound or insufficiently scoped actor must not learn
            // whether a UUID belongs to another tenant. There is no target
            // tenant for a durable audit/rate bucket when the UUID is truly
            // absent, so return the same generic denial without writing.
            return Err(
                if actor_tenant_external_id.is_some() || !actor_allows_recovery {
                    AppError::Forbidden
                } else {
                    AppError::NotFound
                },
            );
        };
        let tenant_id: String = current.try_get("tenant_id")?;
        let tenant_external_id: String = current.try_get("tenant_external_id")?;
        let generation: i64 = current.try_get("credential_generation")?;
        let now = unix_millis();
        if !actor_allows_recovery {
            record_key_credential_recovery_access_audit(
                &mut tx,
                &tenant_id,
                key_id,
                generation,
                actor_service_id,
                "scope_denied",
                now,
            )
            .await?;
            tx.commit().await?;
            return Err(AppError::Forbidden);
        }
        if actor_tenant_external_id
            .is_some_and(|actor_tenant| actor_tenant != tenant_external_id.as_str())
        {
            record_key_credential_recovery_access_audit(
                &mut tx,
                &tenant_id,
                key_id,
                generation,
                actor_service_id,
                "tenant_denied",
                now,
            )
            .await?;
            tx.commit().await?;
            return Err(AppError::Forbidden);
        }
        if current.try_get::<String, _>("status")? != "active" {
            record_key_credential_recovery_access_audit(
                &mut tx,
                &tenant_id,
                key_id,
                generation,
                actor_service_id,
                "inactive",
                now,
            )
            .await?;
            tx.commit().await?;
            return Err(AppError::Forbidden);
        }
        let expected: Option<Vec<u8>> = current.try_get("secret_hash")?;
        let plaintext: Option<String> = current.try_get("secret_plaintext")?;
        let ciphertext: Option<String> = current.try_get("ciphertext")?;
        let Some(expected) = expected else {
            record_key_credential_recovery_access_audit(
                &mut tx,
                &tenant_id,
                key_id,
                generation,
                actor_service_id,
                "unavailable",
                now,
            )
            .await?;
            tx.commit().await?;
            return Err(AppError::NotFound);
        };
        let recovered = if let Some(key) = plaintext {
            KeyCredentialRecoveryEnvelope {
                key_id,
                credential_generation: generation,
                key,
            }
        } else {
            let Some(ciphertext) = ciphertext else {
                record_key_credential_recovery_access_audit(
                    &mut tx,
                    &tenant_id,
                    key_id,
                    generation,
                    actor_service_id,
                    "unavailable",
                    now,
                )
                .await?;
                tx.commit().await?;
                return Err(AppError::NotFound);
            };
            let aad = key_credential_recovery_aad(key_id, generation);
            match open_private_json::<KeyCredentialRecoveryEnvelope>(
                &ciphertext,
                pepper,
                aad.as_bytes(),
            ) {
                Ok(recovered) => recovered,
                Err(_) => {
                    record_key_credential_recovery_access_audit(
                        &mut tx,
                        &tenant_id,
                        key_id,
                        generation,
                        actor_service_id,
                        "integrity_failed",
                        now,
                    )
                    .await?;
                    tx.commit().await?;
                    return Err(AppError::Internal);
                }
            }
        };
        if recovered.key_id != key_id
            || recovered.credential_generation != generation
            || !crypto::verify_credential(&recovered.key, pepper, &expected)
        {
            record_key_credential_recovery_access_audit(
                &mut tx,
                &tenant_id,
                key_id,
                generation,
                actor_service_id,
                "integrity_failed",
                now,
            )
            .await?;
            tx.commit().await?;
            return Err(AppError::Internal);
        }
        // Compatibility path for pre-0084 encrypted envelopes. The key row
        // lock plus active-generation predicate prevents a stale envelope from
        // being persisted after a concurrent rotation.
        sqlx::query(
            "UPDATE key_credentials SET secret_plaintext = $1 WHERE id = $2 AND key_id = $3 AND generation = $4 AND revoked_at IS NULL AND secret_plaintext IS NULL",
        )
        .bind(&recovered.key)
        .bind(current.try_get::<Option<String>, _>("credential_id")?)
        .bind(key_id.to_string())
        .bind(generation)
        .execute(&mut *tx)
        .await?;
        record_key_credential_recovery_audit(
            &mut tx,
            key_id,
            generation,
            "retrieved",
            actor_service_id,
            now,
        )
        .await?;
        record_key_credential_recovery_access_audit(
            &mut tx,
            &tenant_id,
            key_id,
            generation,
            actor_service_id,
            "retrieved",
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(RecoveredClientCredential {
            key_id,
            credential_generation: generation,
            key: recovered.key,
        })
    }
}

async fn record_key_credential_recovery_access_audit(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    key_id: Uuid,
    generation: i64,
    actor_service_id: Option<Uuid>,
    outcome: &str,
    now: i64,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO key_credential_recovery_access_audit (id, tenant_id, key_id, credential_generation, actor_type, actor_service_id, outcome, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(tenant_id)
    .bind(key_id.to_string())
    .bind(generation)
    .bind(if actor_service_id.is_some() {
        "service"
    } else {
        "bootstrap"
    })
    .bind(actor_service_id.map(|service_id| service_id.to_string()))
    .bind(outcome)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(super) async fn store_issued_key_credential_recovery_secret_in_transaction(
    tx: &mut Transaction<'_, Any>,
    issued: &crypto::IssuedCredential,
    generation: i64,
    pepper: &[u8],
    now: i64,
) -> Result<(), AppError> {
    store_key_credential_recovery_secret_in_transaction(
        tx,
        KeyCredentialRecoverySecret {
            credential_id: issued.credential_id,
            key_id: issued.key_id,
            generation,
            credential: &issued.secret,
        },
        pepper,
        None,
        now,
    )
    .await
}

pub(super) async fn remove_key_credential_recovery_secrets_in_transaction(
    tx: &mut Transaction<'_, Any>,
    key_id: Uuid,
    generation: i64,
    now: i64,
) -> Result<(), AppError> {
    let removed = sqlx::query("DELETE FROM key_credential_recovery_secrets WHERE key_id = $1")
        .bind(key_id.to_string())
        .execute(&mut **tx)
        .await?;
    if removed.rows_affected() != 0 {
        record_key_credential_recovery_audit(tx, key_id, generation, "removed", None, now).await?;
    }
    Ok(())
}

fn key_credential_recovery_aad(key_id: Uuid, generation: i64) -> String {
    format!("{KEY_CREDENTIAL_RECOVERY_AAD_PREFIX}/{key_id}/{generation}")
}

async fn store_key_credential_recovery_secret_in_transaction(
    tx: &mut Transaction<'_, Any>,
    secret: KeyCredentialRecoverySecret<'_>,
    pepper: &[u8],
    actor_service_id: Option<Uuid>,
    now: i64,
) -> Result<(), AppError> {
    let KeyCredentialRecoverySecret {
        credential_id,
        key_id,
        generation,
        credential,
    } = secret;
    let aad = key_credential_recovery_aad(key_id, generation);
    let envelope = KeyCredentialRecoveryEnvelope {
        key_id,
        credential_generation: generation,
        key: credential.to_owned(),
    };
    let ciphertext = seal_private_json(&envelope, pepper, aad.as_bytes())?;
    sqlx::query(
        "INSERT INTO key_credential_recovery_secrets (credential_id, key_id, credential_generation, ciphertext, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT(credential_id) DO UPDATE SET ciphertext = excluded.ciphertext, updated_at = excluded.updated_at",
    )
    .bind(credential_id.to_string())
    .bind(key_id.to_string())
    .bind(generation)
    .bind(ciphertext)
    .bind(now)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    record_key_credential_recovery_audit(tx, key_id, generation, "stored", actor_service_id, now)
        .await
}

async fn record_key_credential_recovery_audit(
    tx: &mut Transaction<'_, Any>,
    key_id: Uuid,
    generation: i64,
    action: &str,
    actor_service_id: Option<Uuid>,
    now: i64,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO key_credential_recovery_audit (id, key_id, credential_generation, action, actor_service_id, created_at) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(key_id.to_string())
    .bind(generation)
    .bind(action)
    .bind(actor_service_id.map(|service_id| service_id.to_string()))
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
