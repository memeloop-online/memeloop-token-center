use sqlx::Row;

use super::{Database, DatabaseBackend, unix_millis};
use crate::error::AppError;

impl Database {
    pub async fn plugin_kv_get(
        &self,
        plugin_id: &str,
        key: &str,
    ) -> Result<Option<Vec<u8>>, AppError> {
        validate_plugin_kv_key(plugin_id, key)?;
        let row = sqlx::query("SELECT value FROM plugin_kv WHERE plugin_id = $1 AND key = $2")
            .bind(plugin_id)
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| row.try_get("value").map_err(AppError::from))
            .transpose()
    }

    pub async fn plugin_kv_put(
        &self,
        plugin_id: &str,
        key: &str,
        value: &[u8],
    ) -> Result<(), AppError> {
        const MAX_VALUE_BYTES: usize = 1024 * 1024;
        const MAX_PLUGIN_BYTES: i64 = 16 * 1024 * 1024;
        validate_plugin_kv_key(plugin_id, key)?;
        if value.len() > MAX_VALUE_BYTES {
            return Err(AppError::BadRequest("plugin KV value exceeds 1 MiB".into()));
        }
        let mut transaction = self.pool.begin().await?;
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 734627102948313))")
                .bind(plugin_id)
                .execute(&mut *transaction)
                .await?;
        }
        let usage_query = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT COALESCE(SUM(OCTET_LENGTH(value)), 0) AS total_bytes, COALESCE(MAX(CASE WHEN key = $2 THEN OCTET_LENGTH(value) ELSE 0 END), 0) AS current_bytes FROM plugin_kv WHERE plugin_id = $1"
            }
            DatabaseBackend::Sqlite => {
                "SELECT COALESCE(SUM(LENGTH(value)), 0) AS total_bytes, COALESCE(MAX(CASE WHEN key = $2 THEN LENGTH(value) ELSE 0 END), 0) AS current_bytes FROM plugin_kv WHERE plugin_id = $1"
            }
        };
        let usage = sqlx::query(usage_query)
            .bind(plugin_id)
            .bind(key)
            .fetch_one(&mut *transaction)
            .await?;
        let total_bytes: i64 = usage.try_get("total_bytes")?;
        let current_bytes: i64 = usage.try_get("current_bytes")?;
        let next_bytes = total_bytes
            .saturating_sub(current_bytes)
            .saturating_add(value.len() as i64);
        if next_bytes > MAX_PLUGIN_BYTES {
            return Err(AppError::BadRequest(
                "plugin KV namespace exceeds 16 MiB".into(),
            ));
        }
        sqlx::query(
            "INSERT INTO plugin_kv (plugin_id, key, value, updated_at) VALUES ($1, $2, $3, $4) ON CONFLICT(plugin_id, key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(plugin_id)
        .bind(key)
        .bind(value)
        .bind(unix_millis())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }
}

fn validate_plugin_kv_key(plugin_id: &str, key: &str) -> Result<(), AppError> {
    if plugin_id.is_empty()
        || plugin_id.len() > 64
        || !plugin_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(AppError::BadRequest(
            "plugin id must contain lowercase ASCII letters, digits, or hyphens".into(),
        ));
    }
    if key.is_empty()
        || key.len() > 256
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
    {
        return Err(AppError::BadRequest(
            "plugin KV key must contain 1 to 256 safe ASCII characters".into(),
        ));
    }
    Ok(())
}
