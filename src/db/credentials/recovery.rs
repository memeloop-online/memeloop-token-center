use super::super::*;

const KEY_CREDENTIAL_RECOVERY_AAD_PREFIX: &str = "memeloop-token-center/key-credential-recovery/v1";

#[derive(Deserialize, Serialize)]
struct KeyCredentialRecoveryEnvelope {
    key_id: Uuid,
    credential_generation: i64,
    key: String,
}

impl Database {
    /// Seals a caller-supplied existing credential only when it is exactly the
    /// active credential for this stable key and generation. This supports an
    /// authorized importer that still has old-source access without changing
    /// the key, credential generation, or authentication hash.
    pub async fn store_key_credential_recovery_secret(
        &self,
        key_id: Uuid,
        credential: &str,
        pepper: &[u8],
        actor_service_id: Option<Uuid>,
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
        store_key_credential_recovery_secret_in_transaction(
            &mut tx,
            parse_uuid(current.try_get("credential_id")?)?,
            key_id,
            current.try_get("credential_generation")?,
            credential,
            pepper,
            actor_service_id,
            unix_millis(),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Returns plaintext only through an explicit management action. The stored
    /// envelope and the HMAC are both rechecked against the active key
    /// generation, so an old or transplanted row cannot be replayed.
    pub async fn copy_key_credential(
        &self,
        key_id: Uuid,
        pepper: &[u8],
        actor_service_id: Option<Uuid>,
    ) -> Result<RecoveredClientCredential, AppError> {
        let mut tx = self.begin_write_transaction().await?;
        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT k.status, k.credential_generation, c.secret_hash, r.ciphertext FROM key_records k LEFT JOIN key_credentials c ON c.key_id = k.id AND c.generation = k.credential_generation AND c.revoked_at IS NULL LEFT JOIN key_credential_recovery_secrets r ON r.credential_id = c.id AND r.key_id = k.id AND r.credential_generation = k.credential_generation WHERE k.id = $1 FOR UPDATE OF k"
            }
            DatabaseBackend::Sqlite => {
                "SELECT k.status, k.credential_generation, c.secret_hash, r.ciphertext FROM key_records k LEFT JOIN key_credentials c ON c.key_id = k.id AND c.generation = k.credential_generation AND c.revoked_at IS NULL LEFT JOIN key_credential_recovery_secrets r ON r.credential_id = c.id AND r.key_id = k.id AND r.credential_generation = k.credential_generation WHERE k.id = $1"
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
        let generation: i64 = current.try_get("credential_generation")?;
        let expected: Option<Vec<u8>> = current.try_get("secret_hash")?;
        let ciphertext: Option<String> = current.try_get("ciphertext")?;
        let (expected, ciphertext) = expected.zip(ciphertext).ok_or(AppError::NotFound)?;
        let aad = key_credential_recovery_aad(key_id, generation);
        let recovered = open_private_json::<KeyCredentialRecoveryEnvelope>(
            &ciphertext,
            pepper,
            aad.as_bytes(),
        )?;
        if recovered.key_id != key_id
            || recovered.credential_generation != generation
            || !crypto::verify_credential(&recovered.key, pepper, &expected)
        {
            return Err(AppError::Internal);
        }
        record_key_credential_recovery_audit(
            &mut tx,
            key_id,
            generation,
            "retrieved",
            actor_service_id,
            unix_millis(),
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

pub(super) async fn store_issued_key_credential_recovery_secret_in_transaction(
    tx: &mut Transaction<'_, Any>,
    issued: &crypto::IssuedCredential,
    generation: i64,
    pepper: &[u8],
    now: i64,
) -> Result<(), AppError> {
    store_key_credential_recovery_secret_in_transaction(
        tx,
        issued.credential_id,
        issued.key_id,
        generation,
        &issued.secret,
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
    credential_id: Uuid,
    key_id: Uuid,
    generation: i64,
    credential: &str,
    pepper: &[u8],
    actor_service_id: Option<Uuid>,
    now: i64,
) -> Result<(), AppError> {
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
