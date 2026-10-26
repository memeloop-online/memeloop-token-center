use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;

use super::{AppError, Database};

pub(crate) struct ProbeAccountContext {
    pub(crate) tenant_external_id: String,
    pub(crate) driver: String,
    pub(crate) config: Value,
}

pub(crate) struct ProbeCatalogRow {
    pub(crate) model_id: String,
    pub(crate) protocol: String,
    pub(crate) input_price: Option<i64>,
}

pub(crate) struct ProbeBillingRoute {
    pub(crate) route_id: Uuid,
    pub(crate) public_model: String,
    pub(crate) protocol: String,
}

impl Database {
    pub(crate) async fn upstream_service_probes_due(
        &self,
        healthy_before: i64,
        unhealthy_before: i64,
        limit: i64,
    ) -> Result<Vec<Uuid>, AppError> {
        let rows = sqlx::query(
            "SELECT a.id FROM upstream_accounts a
             LEFT JOIN upstream_service_probes p ON p.upstream_account_id = a.id
             WHERE a.status = 'active'
               AND (
                 p.checked_at IS NULL
                 OR (p.status = 'healthy' AND p.checked_at <= $1)
                 OR (p.status <> 'healthy' AND p.checked_at <= $2)
               )
             ORDER BY COALESCE(p.checked_at, 0), a.id
             LIMIT $3",
        )
        .bind(healthy_before)
        .bind(unhealthy_before)
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
            "source": "model_probe",
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

    /// A completed upstream response is availability evidence. Refresh the
    /// service probe immediately so a later request is not still ranked or
    /// badged from the failure this success already disproved.
    pub(crate) async fn mark_upstream_service_available(
        &self,
        account_id: Uuid,
    ) -> Result<(), AppError> {
        let now = super::unix_millis();
        let account_id = account_id.to_string();
        sqlx::query(
            "INSERT INTO upstream_service_probes (
                 upstream_account_id, status, error_code, latency_ms, checked_at
             ) VALUES ($1, 'healthy', NULL, NULL, $2)
             ON CONFLICT (upstream_account_id) DO UPDATE SET
               status = 'healthy',
               error_code = NULL,
               latency_ms = NULL,
               checked_at = excluded.checked_at",
        )
        .bind(&account_id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        // The exit badge is the same kind of availability. A request that just
        // succeeded through this account is newer evidence than the last
        // independent exit probe.
        sqlx::query(
            "UPDATE transport_proxy_groups
                SET last_probe_status = 'healthy', last_probe_checked_at = $2
              WHERE id = (
                SELECT group_id FROM transport_proxy_bindings WHERE account_id = $1
              )",
        )
        .bind(&account_id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub(crate) async fn note_service_available(&self, account_id: Uuid) {
        if let Err(error) = self.mark_upstream_service_available(account_id).await {
            tracing::warn!(
                %account_id,
                error_category = error.diagnostic_category(),
                "successful upstream attempt did not refresh service availability"
            );
        }
    }

    pub(crate) async fn probe_account_context(
        &self,
        account_id: Uuid,
    ) -> Result<ProbeAccountContext, AppError> {
        let row = sqlx::query(
            "SELECT t.external_id, a.driver, a.config_json
             FROM upstream_accounts a
             JOIN tenants t ON t.id = a.tenant_id
             WHERE a.id = $1 AND a.status = 'active'",
        )
        .bind(account_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        let config: Value = serde_json::from_str(&row.try_get::<String, _>("config_json")?)
            .map_err(|_| AppError::Internal)?;
        Ok(ProbeAccountContext {
            tenant_external_id: row.try_get("external_id")?,
            driver: row.try_get("driver")?,
            config,
        })
    }

    pub(crate) async fn probe_catalog_models(
        &self,
        account_id: Uuid,
    ) -> Result<Vec<ProbeCatalogRow>, AppError> {
        let rows = sqlx::query(
            "SELECT m.model_id, m.protocol,
                    (SELECT MIN(p.input_micros_per_million)
                       FROM model_prices p
                      WHERE p.model = m.model_id) AS input_price
               FROM upstream_model_catalog_state s
               JOIN upstream_accounts a
                 ON a.tenant_id = s.tenant_id
                AND a.id = s.upstream_account_id
                AND a.credential_generation = s.credential_generation
               JOIN upstream_model_catalog_snapshots snapshot
                 ON snapshot.tenant_id = s.tenant_id
                AND snapshot.upstream_account_id = s.upstream_account_id
                AND snapshot.id = s.current_snapshot_id
                AND snapshot.credential_generation = a.credential_generation
               JOIN upstream_models m
                 ON m.tenant_id = s.tenant_id
                AND m.upstream_account_id = s.upstream_account_id
                AND m.snapshot_id = s.current_snapshot_id
              WHERE s.upstream_account_id = $1
                AND s.current_snapshot_id IS NOT NULL
                AND s.status IN ('ready', 'stale', 'syncing')
              ORDER BY m.model_id, m.protocol",
        )
        .bind(account_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(ProbeCatalogRow {
                    model_id: row.try_get("model_id")?,
                    protocol: row.try_get("protocol")?,
                    input_price: row.try_get("input_price")?,
                })
            })
            .collect()
    }

    pub(crate) async fn probe_billing_route(
        &self,
        account_id: Uuid,
        key_id: Uuid,
        model: &str,
    ) -> Result<Option<ProbeBillingRoute>, AppError> {
        let row = sqlx::query(
            "SELECT r.id AS route_id, r.public_model, r.protocol
             FROM model_routes r
             JOIN model_route_eligible_upstream_accounts candidates
               ON candidates.tenant_id = r.tenant_id AND candidates.model_route_id = r.id
             JOIN upstream_accounts a
               ON a.id = candidates.upstream_account_id AND a.tenant_id = r.tenant_id
             WHERE a.id = $1
               AND a.status = 'active'
               AND r.enabled = 1
               AND r.archived_at IS NULL
               AND r.protocol IN ('openai', 'anthropic')
               AND (candidates.upstream_model = $2 OR r.public_model = $2)
               AND (
                 EXISTS (
                   SELECT 1 FROM routing_grants g
                   WHERE g.tenant_id = r.tenant_id AND g.key_id = $3 AND g.model_route_id = r.id
                 )
                 OR EXISTS (
                   SELECT 1 FROM routing_grants g
                   JOIN model_route_group_memberships membership
                     ON membership.tenant_id = g.tenant_id
                    AND membership.route_group_id = g.route_group_id
                   WHERE g.tenant_id = r.tenant_id AND g.key_id = $3
                     AND g.route_group_id IS NOT NULL AND membership.model_route_id = r.id
                 )
               )
             ORDER BY CASE WHEN candidates.upstream_model = $2 THEN 0 ELSE 1 END,
                      r.priority ASC, r.id ASC
             LIMIT 1",
        )
        .bind(account_id.to_string())
        .bind(model)
        .bind(key_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(ProbeBillingRoute {
            route_id: super::parse_uuid(row.try_get("route_id")?)?,
            public_model: row.try_get("public_model")?,
            protocol: row.try_get("protocol")?,
        }))
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
