use super::*;

impl Database {
    /// Only the management-configured filter assistant may delegate this key.
    /// No credential is decrypted, created, or exposed. Normal proxy admission
    /// subsequently rechecks grants, policy, generation and account budgets.
    pub(crate) async fn filter_assistant_identity(
        &self,
        tenant: &str,
        key_id: Uuid,
    ) -> Result<AuthenticatedKey, AppError> {
        let row = sqlx::query("SELECT k.id AS key_id, k.tenant_id, k.principal_id, k.account_id, k.alias, k.currency, k.policy_json, k.credential_generation FROM key_records k JOIN tenants t ON t.id = k.tenant_id AND t.status = 'active' WHERE k.id = $1 AND t.external_id = $2 AND k.status = 'active' AND EXISTS (SELECT 1 FROM key_credentials c WHERE c.key_id = k.id AND c.generation = k.credential_generation AND c.revoked_at IS NULL)")
            .bind(key_id.to_string()).bind(tenant).fetch_optional(&self.pool).await?
            .ok_or(AppError::Forbidden)?;
        Ok(AuthenticatedKey {
            key_id,
            tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
            principal_id: parse_uuid(row.try_get("principal_id")?)?,
            account_id: parse_uuid(row.try_get("account_id")?)?,
            alias: row.try_get("alias")?,
            currency: row.try_get("currency")?,
            credential_generation: row.try_get("credential_generation")?,
            policy: serde_json::from_str(&row.try_get::<String, _>("policy_json")?)
                .map_err(|_| AppError::Internal)?,
        })
    }

    pub(crate) async fn filter_assistant_route(
        &self,
        tenant: &str,
        route_id: Uuid,
    ) -> Result<(String, String), AppError> {
        let row = sqlx::query("SELECT r.public_model, r.protocol FROM model_routes r JOIN tenants t ON t.id = r.tenant_id AND t.status = 'active' WHERE r.id = $1 AND t.external_id = $2 AND r.enabled = 1 AND r.protocol IN ('openai', 'anthropic')")
            .bind(route_id.to_string()).bind(tenant).fetch_optional(&self.pool).await?
            .ok_or_else(|| AppError::BadRequest("filter assistant requires an enabled text route in this tenant".into()))?;
        Ok((row.try_get("public_model")?, row.try_get("protocol")?))
    }

    /// CAS and its immutable audit receipt commit together. Existing plugin KV
    /// storage avoids introducing a migration for this small system policy.
    pub(crate) async fn replace_filter_assistant_settings(
        &self,
        storage_key: &str,
        expected: Option<&[u8]>,
        value: &[u8],
        actor: Option<Uuid>,
    ) -> Result<(), AppError> {
        let mut tx = self.begin_write_transaction().await?;
        let now = unix_millis();
        let changed = if let Some(expected) = expected {
            sqlx::query("UPDATE plugin_kv SET value = $1, updated_at = $2 WHERE plugin_id = 'typed-filter' AND key = $3 AND value = $4")
                .bind(value).bind(now).bind(storage_key).bind(expected).execute(&mut *tx).await?.rows_affected()
        } else {
            sqlx::query("INSERT INTO plugin_kv (plugin_id, key, value, updated_at) VALUES ('typed-filter', $1, $2, $3) ON CONFLICT(plugin_id, key) DO NOTHING")
                .bind(storage_key).bind(value).bind(now).execute(&mut *tx).await?.rows_affected()
        };
        if changed != 1 {
            return Err(AppError::Conflict(
                "filter assistant settings changed; reload before saving".into(),
            ));
        }
        let audit = serde_json::to_vec(&serde_json::json!({"actor_service_id": actor, "settings": serde_json::from_slice::<Value>(value).map_err(|_| AppError::Internal)?, "created_at": now})).map_err(|_| AppError::Internal)?;
        sqlx::query("INSERT INTO plugin_kv (plugin_id, key, value, updated_at) VALUES ('filter-assistant-audit', $1, $2, $3)")
            .bind(Uuid::now_v7().to_string()).bind(audit).bind(now).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
}
