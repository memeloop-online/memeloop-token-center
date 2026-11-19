use super::super::*;

impl Database {
    /// Accept an original value only if it matches the active credential hash.
    /// A hash-only credential cannot be reconstructed or replaced here.
    pub async fn store_key_credential_plaintext(
        &self,
        key_id: Uuid,
        credential: &str,
        pepper: &[u8],
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
                "SELECT k.status, c.id AS credential_id, c.secret_hash FROM key_records k JOIN key_credentials c ON c.key_id = k.id AND c.generation = k.credential_generation AND c.revoked_at IS NULL WHERE k.id = $1 FOR UPDATE OF k"
            }
            DatabaseBackend::Sqlite => {
                "SELECT k.status, c.id AS credential_id, c.secret_hash FROM key_records k JOIN key_credentials c ON c.key_id = k.id AND c.generation = k.credential_generation AND c.revoked_at IS NULL WHERE k.id = $1"
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
        sqlx::query("UPDATE key_credentials SET secret_plaintext = $1 WHERE id = $2")
            .bind(credential)
            .bind(current.try_get::<String, _>("credential_id")?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Replace only the observed generation, preserving the key's identity and
    /// request/account relationships. Existing admitted requests keep their owner.
    pub async fn replace_key_credential_plaintext(
        &self,
        key_id: Uuid,
        credential: &str,
        expected_generation: i64,
        pepper: &[u8],
    ) -> Result<CopiedClientCredential, AppError> {
        if expected_generation < 1
            || credential.len() < 16
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
        let row = sqlx::query(select)
            .bind(key_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
        if row.try_get::<String, _>("status")? != "active" {
            return Err(AppError::Forbidden);
        }
        let current_generation: i64 = row.try_get("credential_generation")?;
        let current_hash: Vec<u8> = row.try_get("secret_hash")?;
        let unchanged = crypto::verify_credential(credential, pepper, &current_hash);
        // A lost response can be read back without rotating a second time.
        let replay = unchanged && expected_generation.checked_add(1) == Some(current_generation);
        if current_generation != expected_generation && !replay {
            return Err(AppError::Conflict(
                "credential generation changed; reload before saving".into(),
            ));
        }
        let generation = if unchanged {
            sqlx::query("UPDATE key_credentials SET secret_plaintext = $1 WHERE id = $2")
                .bind(credential)
                .bind(row.try_get::<String, _>("credential_id")?)
                .execute(&mut *tx)
                .await?;
            current_generation
        } else {
            let generation = current_generation
                .checked_add(1)
                .ok_or_else(|| AppError::Conflict("credential generation is exhausted".into()))?;
            let now = unix_millis();
            let (hash, fingerprint) = crypto::hash_credential(credential, pepper);
            // The global unique hash constraint also fences another key saving
            // the same value concurrently. A conflict rolls back revocation.
            sqlx::query("UPDATE key_credentials SET revoked_at = $1, secret_plaintext = NULL WHERE key_id = $2 AND revoked_at IS NULL")
                .bind(now).bind(key_id.to_string()).execute(&mut *tx).await?;
            let inserted = sqlx::query("INSERT INTO key_credentials (id, key_id, generation, secret_hash, fingerprint, secret_plaintext, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7)")
                .bind(Uuid::now_v7().to_string()).bind(key_id.to_string()).bind(generation).bind(hash).bind(fingerprint).bind(credential).bind(now).execute(&mut *tx).await;
            if let Err(error) = inserted {
                if error
                    .as_database_error()
                    .is_some_and(|error| error.is_unique_violation())
                {
                    return Err(AppError::Conflict(
                        "credential value is already in use".into(),
                    ));
                }
                return Err(error.into());
            }
            sqlx::query(
                "UPDATE key_records SET credential_generation = $1, updated_at = $2 WHERE id = $3",
            )
            .bind(generation)
            .bind(now)
            .bind(key_id.to_string())
            .execute(&mut *tx)
            .await?;
            generation
        };
        tx.commit().await?;
        Ok(CopiedClientCredential {
            key_id,
            credential_generation: generation,
            key: credential.to_owned(),
        })
    }

    /// Copy the stored original only while its generation is active, scoped to
    /// the caller's tenant, and still matches the authentication hash.
    pub async fn copy_key_credential(
        &self,
        key_id: Uuid,
        pepper: &[u8],
        actor_tenant_external_id: Option<&str>,
        actor_allows_copy: bool,
    ) -> Result<CopiedClientCredential, AppError> {
        if !actor_allows_copy {
            return Err(AppError::Forbidden);
        }
        let mut tx = self.begin_write_transaction().await?;
        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT t.external_id AS tenant_external_id, k.status, k.credential_generation, c.secret_hash, c.secret_plaintext FROM key_records k JOIN tenants t ON t.id = k.tenant_id LEFT JOIN key_credentials c ON c.key_id = k.id AND c.generation = k.credential_generation AND c.revoked_at IS NULL WHERE k.id = $1 FOR UPDATE OF k"
            }
            DatabaseBackend::Sqlite => {
                "SELECT t.external_id AS tenant_external_id, k.status, k.credential_generation, c.secret_hash, c.secret_plaintext FROM key_records k JOIN tenants t ON t.id = k.tenant_id LEFT JOIN key_credentials c ON c.key_id = k.id AND c.generation = k.credential_generation AND c.revoked_at IS NULL WHERE k.id = $1"
            }
        };
        let current = sqlx::query(select)
            .bind(key_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(if actor_tenant_external_id.is_some() {
                AppError::Forbidden
            } else {
                AppError::NotFound
            })?;
        let tenant: String = current.try_get("tenant_external_id")?;
        if actor_tenant_external_id.is_some_and(|actor| actor != tenant)
            || current.try_get::<String, _>("status")? != "active"
        {
            return Err(AppError::Forbidden);
        }
        let expected: Option<Vec<u8>> = current.try_get("secret_hash")?;
        let key: Option<String> = current.try_get("secret_plaintext")?;
        let (Some(expected), Some(key)) = (expected, key) else {
            return Err(AppError::NotFound);
        };
        if !crypto::verify_credential(&key, pepper, &expected) {
            return Err(AppError::internal("copy_copy_key_credential_1"));
        }
        let credential_generation = current.try_get("credential_generation")?;
        tx.commit().await?;
        Ok(CopiedClientCredential {
            key_id,
            credential_generation,
            key,
        })
    }
}
