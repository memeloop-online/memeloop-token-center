use super::*;

impl Database {
    /// Retire configuration without destroying stable historical references.
    pub async fn archive_model_route(
        &self,
        route_id: Uuid,
        tenant_external_id: &str,
        expected_updated_at: i64,
    ) -> Result<(), AppError> {
        let mut tx = self.begin_write_transaction().await?;
        let tenant_id = tenant_id(&mut tx, tenant_external_id).await?;
        lock_routing_relation_writes(&mut tx, &tenant_id).await?;
        let row = sqlx::query("SELECT enabled, updated_at, archived_at FROM model_routes WHERE id = $1 AND tenant_id = $2")
            .bind(route_id.to_string()).bind(&tenant_id)
            .fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
        if row.try_get::<Option<i64>, _>("archived_at")?.is_some() {
            tx.commit().await?;
            return Ok(());
        }
        if row.try_get::<i64, _>("enabled")? != 0
            || row.try_get::<i64, _>("updated_at")? != expected_updated_at
        {
            return Err(AppError::Conflict(
                "disable and reload the route before archiving it".into(),
            ));
        }
        let old = route_relation_snapshot(&mut tx, &tenant_id, route_id).await?;
        let now = unix_millis().max(expected_updated_at.saturating_add(1));
        let changed = sqlx::query("UPDATE model_routes SET archived_at = $1, updated_at = $1 WHERE id = $2 AND tenant_id = $3 AND enabled = 0 AND updated_at = $4 AND archived_at IS NULL")
            .bind(now).bind(route_id.to_string()).bind(&tenant_id).bind(expected_updated_at)
            .execute(&mut *tx).await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "reload the route before archiving it".into(),
            ));
        }
        // These are live configuration edges, not request/generation history.
        for sql in [
            "DELETE FROM routing_grants WHERE tenant_id = $1 AND model_route_id = $2",
            "DELETE FROM model_route_group_memberships WHERE tenant_id = $1 AND model_route_id = $2",
            "DELETE FROM model_route_upstream_accounts WHERE tenant_id = $1 AND model_route_id = $2",
            "DELETE FROM model_route_included_provider_groups WHERE tenant_id = $1 AND model_route_id = $2",
            "DELETE FROM model_route_excluded_provider_groups WHERE tenant_id = $1 AND model_route_id = $2",
        ] {
            sqlx::query(sql)
                .bind(&tenant_id)
                .bind(route_id.to_string())
                .execute(&mut *tx)
                .await?;
        }
        finish_route_relation_replace(&mut tx, &tenant_id, old, &[], &[], now).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Explicit audit lookup; ordinary management and picker reads exclude this row.
    pub async fn archived_model_route(
        &self,
        route_id: Uuid,
        tenant_external_id: &str,
    ) -> Result<serde_json::Value, AppError> {
        let row = sqlx::query("SELECT r.public_model, r.upstream_model, r.protocol, r.priority, r.created_at, r.archived_at FROM model_routes r JOIN tenants t ON t.id = r.tenant_id WHERE r.id = $1 AND t.external_id = $2 AND r.archived_at IS NOT NULL")
            .bind(route_id.to_string()).bind(tenant_external_id)
            .fetch_optional(&self.pool).await?.ok_or(AppError::NotFound)?;
        Ok(serde_json::json!({
            "id": route_id,
            "tenant_external_id": tenant_external_id,
            "public_model": row.try_get::<String, _>("public_model")?,
            "upstream_model": row.try_get::<String, _>("upstream_model")?,
            "protocol": row.try_get::<String, _>("protocol")?,
            "priority": row.try_get::<i64, _>("priority")?,
            "enabled": false,
            "created_at": row.try_get::<i64, _>("created_at")?,
            "archived_at": row.try_get::<i64, _>("archived_at")?,
        }))
    }
}
