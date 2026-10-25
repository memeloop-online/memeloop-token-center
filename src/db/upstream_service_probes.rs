use sqlx::Row;
use uuid::Uuid;

use super::{AppError, Database};

impl Database {
    pub(crate) async fn upstream_service_probes_due(
        &self,
        older_than: i64,
        limit: i64,
    ) -> Result<Vec<Uuid>, AppError> {
        let rows = sqlx::query(
            "SELECT a.id FROM upstream_accounts a
             LEFT JOIN upstream_service_probes p ON p.upstream_account_id = a.id
             WHERE a.status = 'active'
               AND (p.checked_at IS NULL OR p.checked_at <= $1)
             ORDER BY COALESCE(p.checked_at, 0), a.id
             LIMIT $2",
        )
        .bind(older_than)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                let id: String = row.try_get("id")?;
                Uuid::parse_str(&id).map_err(|_| AppError::Internal)
            })
            .collect()
    }

    pub(crate) async fn upstream_service_probe(
        &self,
        account_id: Uuid,
    ) -> Result<Option<serde_json::Value>, AppError> {
        let row = sqlx::query(
            "SELECT status, error_code, latency_ms, checked_at FROM upstream_service_probes WHERE upstream_account_id = $1",
        )
        .bind(account_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let status: String = row.try_get("status")?;
        let error_code: Option<String> = row.try_get("error_code")?;
        let latency_ms: Option<i64> = row.try_get("latency_ms")?;
        let checked_at: i64 = row.try_get("checked_at")?;
        Ok(Some(serde_json::json!({
            "account_id": account_id,
            "status": status,
            "error_code": error_code,
            "latency_ms": latency_ms,
            "source": "connection_probe",
            "checked_at": checked_at
        })))
    }

    pub(crate) async fn store_upstream_service_probe(
        &self,
        account_id: Uuid,
        body: &serde_json::Value,
    ) -> Result<(), AppError> {
        let status = body
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or("unhealthy");
        let error_code = body
            .get("error_code")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty());
        let latency_ms = body.get("latency_ms").and_then(|value| value.as_i64());
        let checked_at = body
            .get("checked_at")
            .and_then(|value| value.as_i64())
            .unwrap_or_else(super::unix_millis);
        sqlx::query(
            "INSERT INTO upstream_service_probes (upstream_account_id, status, error_code, latency_ms, checked_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (upstream_account_id) DO UPDATE SET
               status = excluded.status,
               error_code = excluded.error_code,
               latency_ms = excluded.latency_ms,
               checked_at = excluded.checked_at",
        )
        .bind(account_id.to_string())
        .bind(status)
        .bind(error_code)
        .bind(latency_ms)
        .bind(checked_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub(crate) async fn transport_group_bound_accounts(
        &self,
        group_id: Uuid,
    ) -> Result<Vec<Uuid>, AppError> {
        let rows = sqlx::query(
            "SELECT account_id FROM transport_proxy_bindings WHERE group_id = $1 ORDER BY account_id",
        )
        .bind(group_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                let id: String = row.try_get("account_id")?;
                Uuid::parse_str(&id).map_err(|_| AppError::Internal)
            })
            .collect()
    }
}
