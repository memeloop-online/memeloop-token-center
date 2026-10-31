use super::super::*;

impl Database {
    pub async fn update_service_token(
        &self,
        service_id: Uuid,
        name: Option<&str>,
        scopes: Option<&[String]>,
        tenant_external_id: Option<Option<&str>>,
    ) -> Result<ServiceTokenView, AppError> {
        if name.is_none() && scopes.is_none() && tenant_external_id.is_none() {
            return Err(AppError::BadRequest(
                "at least one service token field is required".into(),
            ));
        }
        let name = name.map(str::trim);
        if name.is_some_and(|value| {
            value.is_empty() || value.len() > 120 || value.chars().any(char::is_control)
        }) {
            return Err(AppError::BadRequest(
                "service token name must contain 1 to 120 non-control characters".into(),
            ));
        }
        if let Some(scopes) = scopes {
            super::service_tokens::validate_service_scopes(scopes)?;
        }
        let tenant_external_id = tenant_external_id.map(|tenant| tenant.map(str::trim));
        if tenant_external_id.flatten().is_some_and(|value| {
            value.is_empty() || value.len() > 200 || value.chars().any(char::is_control)
        }) {
            return Err(AppError::BadRequest(
                "tenant_external_id must contain 1 to 200 non-control characters".into(),
            ));
        }
        let mut tx = self.begin_write_transaction().await?;
        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT name, status, credential_generation, created_at FROM service_principals WHERE id = $1 FOR UPDATE"
            }
            DatabaseBackend::Sqlite => {
                "SELECT name, status, credential_generation, created_at FROM service_principals WHERE id = $1"
            }
        };
        let current = sqlx::query(select)
            .bind(service_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
        let status: String = current.try_get("status")?;
        if status == "revoked" {
            return Err(AppError::Conflict(
                "a revoked service credential cannot be edited".into(),
            ));
        }
        let credential = sqlx::query("SELECT fingerprint, CASE WHEN secret_plaintext IS NOT NULL THEN 1 ELSE 0 END AS credential_copy_available, scopes_json, tenant_external_id FROM service_credentials WHERE service_principal_id = $1 AND generation = $2 AND revoked_at IS NULL")
            .bind(service_id.to_string())
            .bind(current.try_get::<i64, _>("credential_generation")?)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
        let name = name.map(str::to_owned).unwrap_or(current.try_get("name")?);
        let scopes_json: String = credential.try_get("scopes_json")?;
        let scopes = match scopes {
            Some(scopes) => scopes.to_vec(),
            None => serde_json::from_str(&scopes_json)
                .map_err(|_| AppError::internal("service_token_scopes_decode"))?,
        };
        super::service_tokens::validate_service_scopes(&scopes)?;
        let tenant_external_id = match tenant_external_id {
            Some(tenant) => tenant.map(str::to_owned),
            None => credential.try_get("tenant_external_id")?,
        };
        let updated_at = unix_millis();
        sqlx::query("UPDATE service_principals SET name = $1, updated_at = $2 WHERE id = $3")
            .bind(&name)
            .bind(updated_at)
            .bind(service_id.to_string())
            .execute(&mut *tx)
            .await?;
        let changed = sqlx::query("UPDATE service_credentials SET scopes_json = $1, tenant_external_id = $2 WHERE service_principal_id = $3 AND generation = $4 AND revoked_at IS NULL")
            .bind(serde_json::to_string(&scopes).map_err(|_| AppError::internal("service_token_scopes_encode"))?)
            .bind(&tenant_external_id)
            .bind(service_id.to_string())
            .bind(current.try_get::<i64, _>("credential_generation")?)
            .execute(&mut *tx)
            .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::NotFound);
        }
        let updated = ServiceTokenView {
            service_id,
            name,
            status: status.clone(),
            credential_generation: current.try_get("credential_generation")?,
            fingerprint: credential.try_get("fingerprint")?,
            credential_copy_available: status == "active"
                && credential.try_get::<i64, _>("credential_copy_available")? != 0,
            scopes,
            tenant_external_id,
            created_at: current.try_get("created_at")?,
            updated_at,
        };
        tx.commit().await?;
        Ok(updated)
    }

    /// Removes identities from the working directory without erasing request
    /// attribution, balances, reservations, or ledger records. The whole
    /// explicit selection is tenant-checked and revoked in one transaction.
    pub async fn delete_client_credentials(
        &self,
        tenant: &str,
        key_ids: &[Uuid],
    ) -> Result<Vec<Uuid>, AppError> {
        if key_ids.is_empty() || key_ids.len() > 100 {
            return Err(AppError::BadRequest(
                "select between 1 and 100 credentials".into(),
            ));
        }
        let mut key_ids = key_ids.to_vec();
        key_ids.sort_unstable();
        key_ids.dedup();
        let mut tx = self.begin_write_transaction().await?;
        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT k.credential_generation FROM key_records k JOIN tenants t ON t.id = k.tenant_id WHERE k.id = $1 AND t.external_id = $2 FOR UPDATE OF k"
            }
            DatabaseBackend::Sqlite => {
                "SELECT k.credential_generation FROM key_records k JOIN tenants t ON t.id = k.tenant_id WHERE k.id = $1 AND t.external_id = $2"
            }
        };
        let now = unix_millis();
        for key_id in &key_ids {
            sqlx::query(select)
                .bind(key_id.to_string())
                .bind(tenant)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(AppError::NotFound)?;
            sqlx::query("UPDATE key_records SET status = 'revoked', archived_at = COALESCE(archived_at, $1), updated_at = $1, issued_key_ciphertext = NULL WHERE id = $2")
                .bind(now).bind(key_id.to_string()).execute(&mut *tx).await?;
            sqlx::query("UPDATE key_credentials SET revoked_at = COALESCE(revoked_at, $1), secret_plaintext = NULL WHERE key_id = $2")
                .bind(now).bind(key_id.to_string()).execute(&mut *tx).await?;
            sqlx::query("UPDATE credential_rotation_replays SET response_ciphertext = NULL WHERE resource_kind = 'key' AND resource_id = $1")
                .bind(key_id.to_string()).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(key_ids)
    }
}
