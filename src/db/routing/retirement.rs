use super::*;

impl Database {
    /// Remove only explicitly reviewed, disabled account candidates. The route
    /// and all customer authorization edges remain, even with no candidates.
    pub async fn retire_model_route_upstreams(
        &self,
        route_id: Uuid,
        tenant_external_id: &str,
        account_ids: Vec<Uuid>,
        expected_updated_at: i64,
        expected_grant_revision: i64,
    ) -> Result<RouteRoutingView, AppError> {
        let account_ids = bounded_unique_ids(account_ids, "retired upstream accounts")?;
        if account_ids.is_empty() {
            return Err(AppError::BadRequest(
                "at least one retired upstream account is required".into(),
            ));
        }
        let mut tx = self.begin_write_transaction().await?;
        let tenant_id = tenant_id(&mut tx, tenant_external_id).await?;
        lock_routing_relation_writes(&mut tx, &tenant_id).await?;
        let route = sqlx::query(
            "SELECT enabled, updated_at FROM model_routes WHERE id = $1 AND tenant_id = $2 AND archived_at IS NULL",
        )
        .bind(route_id.to_string())
        .bind(&tenant_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        if route.try_get::<i64, _>("enabled")? != 0
            || route.try_get::<i64, _>("updated_at")? != expected_updated_at
        {
            return Err(AppError::Conflict(
                "disable and reload the model route before retiring upstreams".into(),
            ));
        }
        compare_and_bump_route_grant_revision(
            &mut tx,
            &tenant_id,
            route_id,
            expected_grant_revision,
            false,
        )
        .await?;
        // Provider-group membership writers do not all take the routing
        // relation lock. Reject all included groups, even currently empty ones,
        // so a concurrent membership addition cannot resurrect a candidate.
        if sqlx::query(
            "SELECT 1 FROM model_route_included_provider_groups \
             WHERE tenant_id = $1 AND model_route_id = $2 LIMIT 1",
        )
        .bind(&tenant_id)
        .bind(route_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .is_some()
        {
            return Err(AppError::Conflict(
                "routes with included provider groups require separate retirement review".into(),
            ));
        }
        for account_id in &account_ids {
            // A conditional no-op UPDATE locks the account until commit on both
            // supported databases, preventing a concurrent activation from
            // passing between the disabled check and candidate removal.
            let account = sqlx::query(
                "UPDATE upstream_accounts SET status = status \
                 WHERE id = $1 AND tenant_id = $2 AND status = 'disabled'",
            )
            .bind(account_id.to_string())
            .bind(&tenant_id)
            .execute(&mut *tx)
            .await?;
            if account.rows_affected() != 1 {
                return Err(AppError::Conflict(
                    "disable the upstream account before retiring its route candidates".into(),
                ));
            }
            let removed = sqlx::query(
                "DELETE FROM model_route_upstream_accounts \
                 WHERE tenant_id = $1 AND model_route_id = $2 AND upstream_account_id = $3",
            )
            .bind(&tenant_id)
            .bind(route_id.to_string())
            .bind(account_id.to_string())
            .execute(&mut *tx)
            .await?;
            if removed.rows_affected() != 1 {
                return Err(AppError::Conflict(
                    "reload the exact route candidates before retirement".into(),
                ));
            }
        }
        let remaining = sqlx::query(
            "SELECT upstream_account_id AS id FROM model_route_upstream_accounts \
             WHERE tenant_id = $1 AND model_route_id = $2 ORDER BY upstream_account_id",
        )
        .bind(&tenant_id)
        .bind(route_id.to_string())
        .fetch_all(&mut *tx)
        .await?;
        let compatibility_id = remaining
            .first()
            .map(|row| parse_uuid(row.try_get::<String, _>("id")?))
            .transpose()?
            .unwrap_or_else(Uuid::nil);
        let now = unix_millis().max(expected_updated_at.saturating_add(1));
        let changed = sqlx::query(
            "UPDATE model_routes SET upstream_account_id = $1, updated_at = $2 \
             WHERE id = $3 AND tenant_id = $4 AND enabled = 0 AND updated_at = $5",
        )
        .bind(compatibility_id.to_string())
        .bind(now)
        .bind(route_id.to_string())
        .bind(&tenant_id)
        .bind(expected_updated_at)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict("reload the retired model route".into()));
        }
        tx.commit().await?;
        self.route_routing_view(route_id, &tenant_id, now).await
    }
}
