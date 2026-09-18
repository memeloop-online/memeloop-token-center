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
        sqlx::query("UPDATE key_credentials SET secret_plaintext = $1 WHERE id = $2 AND secret_plaintext IS NULL")
            .bind(credential)
            .bind(current.try_get::<String, _>("credential_id")?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
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
            return Err(AppError::Internal);
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
