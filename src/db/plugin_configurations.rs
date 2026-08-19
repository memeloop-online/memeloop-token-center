use super::*;
use crate::plugin::{PutPluginConfigurationInput, StoredPluginConfiguration};
use futures_util::TryStreamExt;

impl Database {
    pub(crate) async fn visit_plugin_configurations(
        &self,
        mut visit: impl FnMut(StoredPluginConfiguration) -> Result<(), AppError>,
    ) -> Result<(), AppError> {
        let mut rows = sqlx::query(
            "SELECT plugin_id, tenant_id, value_json, schema_digest, version, updated_at FROM plugin_configurations ORDER BY plugin_id ASC, scope_key ASC",
        )
        .fetch(&self.pool);
        while let Some(row) = rows.try_next().await? {
            visit(stored_configuration(row)?)?;
        }
        Ok(())
    }

    pub async fn plugin_configuration_tenant_id(
        &self,
        tenant_external_id: &str,
    ) -> Result<Uuid, AppError> {
        let tenant_external_id = tenant_external_id.trim();
        if tenant_external_id.is_empty() || tenant_external_id.len() > 200 {
            return Err(AppError::BadRequest(
                "tenant_external_id must contain 1 to 200 characters".into(),
            ));
        }
        let id: String = sqlx::query_scalar("SELECT id FROM tenants WHERE external_id = $1")
            .bind(tenant_external_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::NotFound)?;
        parse_uuid(id)
    }

    pub async fn plugin_configuration_layers(
        &self,
        tenant_id: Uuid,
    ) -> Result<Vec<StoredPluginConfiguration>, AppError> {
        let rows = sqlx::query(
            "SELECT plugin_id, tenant_id, value_json, schema_digest, version, updated_at FROM plugin_configurations WHERE scope_kind = 'global' OR tenant_id = $1 ORDER BY plugin_id ASC, scope_kind ASC",
        )
        .bind(tenant_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(stored_configuration).collect()
    }

    pub async fn plugin_configuration_for_scope(
        &self,
        plugin_id: &str,
        tenant_id: Option<Uuid>,
    ) -> Result<Option<StoredPluginConfiguration>, AppError> {
        let scope_key = plugin_configuration_scope_key(tenant_id);
        sqlx::query(
            "SELECT plugin_id, tenant_id, value_json, schema_digest, version, updated_at FROM plugin_configurations WHERE plugin_id = $1 AND scope_key = $2",
        )
        .bind(plugin_id)
        .bind(scope_key)
        .fetch_optional(&self.pool)
        .await?
        .map(stored_configuration)
        .transpose()
    }

    pub async fn put_plugin_configuration(
        &self,
        input: PutPluginConfigurationInput,
    ) -> Result<StoredPluginConfiguration, AppError> {
        if input.expected_version < 0
            || input.idempotency_key.is_empty()
            || input.idempotency_key.len() > 200
            || input.idempotency_key.chars().any(char::is_control)
            || input.request_hash.len() != 64
            || !input
                .request_hash
                .bytes()
                .all(|value| value.is_ascii_hexdigit())
            || input.schema_digest.len() != 64
            || !input
                .schema_digest
                .bytes()
                .all(|value| value.is_ascii_hexdigit())
        {
            return Err(AppError::BadRequest(
                "invalid plugin configuration concurrency metadata".into(),
            ));
        }
        let value_json = serde_json::to_string(&input.value).map_err(|_| AppError::Internal)?;
        if value_json.len() > 1024 * 1024 {
            return Err(AppError::BadRequest(
                "plugin configuration exceeds the 1 MiB limit".into(),
            ));
        }
        let scope_key = plugin_configuration_scope_key(input.tenant_id);
        let scope_kind = if input.tenant_id.is_some() {
            "tenant"
        } else {
            "global"
        };
        let now = unix_millis();
        let mut transaction = self.pool.begin().await?;

        let claimed = sqlx::query(
            "INSERT INTO plugin_configuration_operations (plugin_id, scope_key, idempotency_key, request_hash, created_at) VALUES ($1, $2, $3, $4, $5) ON CONFLICT(plugin_id, scope_key, idempotency_key) DO NOTHING",
        )
        .bind(&input.plugin_id)
        .bind(&scope_key)
        .bind(&input.idempotency_key)
        .bind(&input.request_hash)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        if claimed.rows_affected() == 0 {
            let replay = sqlx::query(
                "SELECT request_hash, result_version, result_value_json, result_schema_digest, result_updated_at FROM plugin_configuration_operations WHERE plugin_id = $1 AND scope_key = $2 AND idempotency_key = $3",
            )
            .bind(&input.plugin_id)
            .bind(&scope_key)
            .bind(&input.idempotency_key)
            .fetch_one(&mut *transaction)
            .await?;
            if replay.try_get::<String, _>("request_hash")? != input.request_hash {
                return Err(AppError::Conflict(
                    "idempotency key was already used for a different plugin configuration".into(),
                ));
            }
            let result_version: Option<i64> = replay.try_get("result_version")?;
            let result_value_json: Option<String> = replay.try_get("result_value_json")?;
            let result_schema_digest: Option<String> = replay.try_get("result_schema_digest")?;
            let result_updated_at: Option<i64> = replay.try_get("result_updated_at")?;
            let (version, value_json, schema_digest, updated_at) = match (
                result_version,
                result_value_json,
                result_schema_digest,
                result_updated_at,
            ) {
                (Some(version), Some(value), Some(digest), Some(updated_at)) => {
                    (version, value, digest, updated_at)
                }
                _ => {
                    return Err(AppError::Conflict(
                        "configuration update is in progress".into(),
                    ));
                }
            };
            transaction.commit().await?;
            return Ok(StoredPluginConfiguration {
                plugin_id: input.plugin_id,
                tenant_id: input.tenant_id,
                value: serde_json::from_str(&value_json).map_err(|_| AppError::Internal)?,
                schema_digest,
                version,
                updated_at,
            });
        }

        let next_version = input.expected_version.saturating_add(1);
        let changed = if input.expected_version == 0 {
            sqlx::query(
                "INSERT INTO plugin_configurations (plugin_id, scope_key, scope_kind, tenant_id, value_json, schema_digest, version, updated_at) VALUES ($1, $2, $3, $4, $5, $6, 1, $7) ON CONFLICT(plugin_id, scope_key) DO NOTHING",
            )
            .bind(&input.plugin_id)
            .bind(&scope_key)
            .bind(scope_kind)
            .bind(input.tenant_id.map(|value| value.to_string()))
            .bind(&value_json)
            .bind(&input.schema_digest)
            .bind(now)
            .execute(&mut *transaction)
            .await?
            .rows_affected()
        } else {
            sqlx::query(
                "UPDATE plugin_configurations SET value_json = $1, schema_digest = $2, version = version + 1, updated_at = $3 WHERE plugin_id = $4 AND scope_key = $5 AND version = $6",
            )
            .bind(&value_json)
            .bind(&input.schema_digest)
            .bind(now)
            .bind(&input.plugin_id)
            .bind(&scope_key)
            .bind(input.expected_version)
            .execute(&mut *transaction)
            .await?
            .rows_affected()
        };
        if changed != 1 {
            return Err(AppError::Conflict(
                "plugin configuration version changed".into(),
            ));
        }
        let recorded = sqlx::query(
            "UPDATE plugin_configuration_operations SET result_version = $1, result_value_json = $2, result_schema_digest = $3, result_updated_at = $4 WHERE plugin_id = $5 AND scope_key = $6 AND idempotency_key = $7 AND request_hash = $8 AND result_version IS NULL",
        )
        .bind(next_version)
        .bind(&value_json)
        .bind(&input.schema_digest)
        .bind(now)
        .bind(&input.plugin_id)
        .bind(&scope_key)
        .bind(&input.idempotency_key)
        .bind(&input.request_hash)
        .execute(&mut *transaction)
        .await?;
        if recorded.rows_affected() != 1 {
            return Err(AppError::Internal);
        }
        transaction.commit().await?;
        Ok(StoredPluginConfiguration {
            plugin_id: input.plugin_id,
            tenant_id: input.tenant_id,
            value: input.value,
            schema_digest: input.schema_digest,
            version: next_version,
            updated_at: now,
        })
    }
}

fn plugin_configuration_scope_key(tenant_id: Option<Uuid>) -> String {
    tenant_id.map_or_else(|| "global".to_owned(), |id| format!("tenant:{id}"))
}

fn stored_configuration(row: AnyRow) -> Result<StoredPluginConfiguration, AppError> {
    let tenant_id: Option<String> = row.try_get("tenant_id")?;
    Ok(StoredPluginConfiguration {
        plugin_id: row.try_get("plugin_id")?,
        tenant_id: tenant_id.map(parse_uuid).transpose()?,
        value: serde_json::from_str(&row.try_get::<String, _>("value_json")?)
            .map_err(|_| AppError::Internal)?,
        schema_digest: row.try_get("schema_digest")?,
        version: row.try_get("version")?,
        updated_at: row.try_get("updated_at")?,
    })
}
