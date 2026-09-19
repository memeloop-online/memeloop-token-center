use std::collections::BTreeSet;

use serde::Serialize;
use sqlx::Row;
use uuid::Uuid;

use super::super::{AppError, Database, DatabaseBackend, DiscoveredUpstreamModel, unix_millis};
use super::associations::tenant_id;
use super::grant_revisions::lock_routing_relation_writes;
use super::input::validate_route_fields;

#[derive(Debug, Default, Serialize)]
pub struct ManagedModelRouteSyncResult {
    pub added: usize,
    pub disabled: usize,
    pub restored: usize,
    pub unchanged: usize,
    pub skipped: usize,
    pub warnings: Vec<String>,
}

impl ManagedModelRouteSyncResult {
    pub fn skipped(reason: &str) -> Self {
        Self {
            skipped: 1,
            warnings: vec![reason.to_owned()],
            ..Self::default()
        }
    }
}

impl Database {
    /// Reconcile only a confirmed, complete discovery supplied by the explicit
    /// sync service. The current committed catalog must still match it exactly.
    /// Background refreshes never call this method. No grants are created or
    /// replaced, and existing manual routes are neither adopted nor modified.
    pub async fn reconcile_managed_model_routes(
        &self,
        account_id: Uuid,
        tenant_external_id: &str,
        credential_generation: i64,
        complete_models: &[DiscoveredUpstreamModel],
    ) -> Result<ManagedModelRouteSyncResult, AppError> {
        let mut tx = self.begin_write_transaction().await?;
        let tenant_id = tenant_id(&mut tx, tenant_external_id).await?;
        lock_routing_relation_writes(&mut tx, &tenant_id).await?;
        // Lock order agrees with catalog replacement and route editing.
        let account_sql = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT credential_generation, status FROM upstream_accounts WHERE id = $1 AND tenant_id = $2 FOR UPDATE"
            }
            DatabaseBackend::Sqlite => {
                "SELECT credential_generation, status FROM upstream_accounts WHERE id = $1 AND tenant_id = $2"
            }
        };
        let account = sqlx::query(account_sql)
            .bind(account_id.to_string())
            .bind(&tenant_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
        if complete_models.is_empty() {
            return Ok(ManagedModelRouteSyncResult::skipped(
                "empty_catalog_protected",
            ));
        }
        if account.try_get::<i64, _>("credential_generation")? != credential_generation
            || account.try_get::<String, _>("status")? != "active"
        {
            return Ok(ManagedModelRouteSyncResult::skipped("account_changed"));
        }
        let catalog = sqlx::query("SELECT current_snapshot_id FROM upstream_model_catalog_state WHERE upstream_account_id = $1 AND tenant_id = $2 AND credential_generation = $3 AND status = 'ready' AND expires_at > $4")
            .bind(account_id.to_string()).bind(&tenant_id).bind(credential_generation)
            .bind(unix_millis()).fetch_optional(&mut *tx).await?;
        let Some(catalog) = catalog else {
            return Ok(ManagedModelRouteSyncResult::skipped("catalog_not_ready"));
        };
        let snapshot_id: String = catalog.try_get("current_snapshot_id")?;
        let rows = sqlx::query("SELECT model_id, protocol FROM upstream_models WHERE snapshot_id = $1 AND tenant_id = $2 AND upstream_account_id = $3")
            .bind(snapshot_id).bind(&tenant_id).bind(account_id.to_string())
            .fetch_all(&mut *tx).await?;
        let current = rows
            .iter()
            .map(|row| {
                Ok((
                    row.try_get::<String, _>("model_id")?,
                    row.try_get::<String, _>("protocol")?,
                ))
            })
            .collect::<Result<BTreeSet<_>, sqlx::Error>>()?;
        let expected = complete_models
            .iter()
            .map(|model| (model.model_id.clone(), model.protocol.clone()))
            .collect::<BTreeSet<_>>();
        if current != expected {
            return Ok(ManagedModelRouteSyncResult::skipped("catalog_changed"));
        }
        let mut result = ManagedModelRouteSyncResult::default();
        let mut desired = BTreeSet::new();
        for (model, protocol) in expected {
            // An OpenAI-style directory omits protocol metadata. Do not invent
            // additional Anthropic/audio/media routes for such wildcard entries.
            let protocol = if protocol == "any" {
                "openai".to_owned()
            } else {
                protocol
            };
            if validate_route_fields(&model, &model, &protocol, 0).is_err() || model.trim() != model
            {
                result.skipped += 1;
                if !result
                    .warnings
                    .iter()
                    .any(|warning| warning == "unsupported_route_model_or_protocol")
                {
                    result
                        .warnings
                        .push("unsupported_route_model_or_protocol".into());
                }
                continue;
            }
            desired.insert((model, protocol));
        }
        let owned = sqlx::query("SELECT managed.upstream_model, managed.protocol, managed.model_route_id, managed.managed_updated_at, managed.disabled_reason, managed.operator_override, route.updated_at, route.enabled, route.archived_at FROM managed_model_routes managed LEFT JOIN model_routes route ON route.id = managed.model_route_id AND route.tenant_id = managed.tenant_id WHERE managed.tenant_id = $1 AND managed.upstream_account_id = $2")
            .bind(&tenant_id).bind(account_id.to_string()).fetch_all(&mut *tx).await?;
        for row in owned {
            let model: String = row.try_get("upstream_model")?;
            let protocol: String = row.try_get("protocol")?;
            let present = desired.remove(&(model.clone(), protocol.clone()));
            let route_id: Option<String> = row.try_get("model_route_id")?;
            let updated_at: Option<i64> = row.try_get("updated_at")?;
            let managed_updated_at: i64 = row.try_get("managed_updated_at")?;
            let archived_at: Option<i64> = row.try_get("archived_at")?;
            let overridden = row.try_get::<i64, _>("operator_override")? != 0;
            if overridden
                || route_id.is_none()
                || updated_at != Some(managed_updated_at)
                || archived_at.is_some()
            {
                // Persist the hand-off even if an operator later happens to
                // restore the original fields. Deleted rows remain tombstones.
                sqlx::query("UPDATE managed_model_routes SET operator_override = 1 WHERE tenant_id = $1 AND upstream_account_id = $2 AND upstream_model = $3 AND protocol = $4")
                    .bind(&tenant_id).bind(account_id.to_string()).bind(&model).bind(&protocol)
                    .execute(&mut *tx).await?;
                result.skipped += 1;
                if !result
                    .warnings
                    .iter()
                    .any(|warning| warning == "operator_route_preserved")
                {
                    result.warnings.push("operator_route_preserved".into());
                }
                continue;
            }
            let enabled = row.try_get::<i64, _>("enabled")? != 0;
            let reason: Option<String> = row.try_get("disabled_reason")?;
            let next = if !present && enabled {
                result.disabled += 1;
                Some((false, Some("catalog_missing")))
            } else if present && !enabled && reason.as_deref() == Some("catalog_missing") {
                result.restored += 1;
                Some((true, None))
            } else {
                result.unchanged += 1;
                None
            };
            if let Some((enabled, reason)) = next {
                let now = unix_millis().max(managed_updated_at.saturating_add(1));
                let changed = sqlx::query("UPDATE model_routes SET enabled = $1, updated_at = $2 WHERE id = $3 AND tenant_id = $4 AND updated_at = $5 AND archived_at IS NULL")
                    .bind(i64::from(enabled)).bind(now).bind(&route_id).bind(&tenant_id).bind(managed_updated_at)
                    .execute(&mut *tx).await?;
                if changed.rows_affected() != 1 {
                    return Err(AppError::Conflict(
                        "model route changed during synchronization".into(),
                    ));
                }
                sqlx::query("UPDATE managed_model_routes SET managed_updated_at = $1, disabled_reason = $2 WHERE model_route_id = $3 AND tenant_id = $4")
                    .bind(now).bind(reason).bind(&route_id).bind(&tenant_id).execute(&mut *tx).await?;
            }
        }
        for (model, protocol) in desired {
            let route_id = Uuid::now_v7().to_string();
            let now = unix_millis();
            // Never use equivalence/adoption here: an existing manual candidate
            // must retain its independent identity and lifecycle.
            sqlx::query("INSERT INTO model_routes (id, tenant_id, public_model, upstream_account_id, upstream_model, protocol, priority, enabled, created_at, updated_at) VALUES ($1, $2, $3, $4, $3, $5, 0, 1, $6, $6)")
                .bind(&route_id).bind(&tenant_id).bind(&model).bind(account_id.to_string()).bind(&protocol).bind(now)
                .execute(&mut *tx).await?;
            sqlx::query("INSERT INTO model_route_upstream_accounts (tenant_id, model_route_id, upstream_account_id, upstream_model, scheduling_weight, created_at, catalog_policy) VALUES ($1, $2, $3, $4, 100, $5, 'required')")
                .bind(&tenant_id).bind(&route_id).bind(account_id.to_string()).bind(&model).bind(now)
                .execute(&mut *tx).await?;
            sqlx::query("INSERT INTO managed_model_routes (tenant_id, upstream_account_id, upstream_model, protocol, model_route_id, managed_updated_at) VALUES ($1, $2, $3, $4, $5, $6)")
                .bind(&tenant_id).bind(account_id.to_string()).bind(&model).bind(&protocol).bind(&route_id).bind(now)
                .execute(&mut *tx).await?;
            result.added += 1;
        }
        tx.commit().await?;
        Ok(result)
    }
}
