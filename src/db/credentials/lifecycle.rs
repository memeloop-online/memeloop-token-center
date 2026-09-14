use super::super::*;
use super::recovery::remove_key_credential_recovery_secrets_in_transaction;

impl Database {
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
            let row = sqlx::query(select)
                .bind(key_id.to_string())
                .bind(tenant)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(AppError::NotFound)?;
            sqlx::query("UPDATE key_records SET status = 'revoked', archived_at = COALESCE(archived_at, $1), updated_at = $1, issued_key_ciphertext = NULL WHERE id = $2")
                .bind(now).bind(key_id.to_string()).execute(&mut *tx).await?;
            sqlx::query("UPDATE key_credentials SET revoked_at = COALESCE(revoked_at, $1), secret_plaintext = NULL WHERE key_id = $2")
                .bind(now).bind(key_id.to_string()).execute(&mut *tx).await?;
            remove_key_credential_recovery_secrets_in_transaction(
                &mut tx,
                *key_id,
                row.try_get("credential_generation")?,
                now,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(key_ids)
    }
}
