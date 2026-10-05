use super::super::*;

pub(super) async fn enqueue_terminal_projection_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    request_id: &str,
) -> Result<(), AppError> {
    let inserted = sqlx::query(
        "INSERT INTO terminal_projection_outbox (request_id, reservation_id, tenant_id, key_id, created_at, completed_at, model, protocol, status_code, error_code, upstream_account_id, model_route_id, duration_ms, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, generation_units, billing_unit, service_tier, currency, cost_micros, usage_basis, session_id, terminal_cause_code)
         SELECT id, reservation_id, tenant_id, key_id, created_at, completed_at, model, protocol, status_code, COALESCE(error_code, ''), COALESCE(upstream_account_id, ''), COALESCE(model_route_id, ''), COALESCE(duration_ms, 0), input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, billed_units, billing_unit, service_tier, currency, cost_micros, COALESCE(usage_basis, ''), COALESCE(conversation_cluster_id, 'unlinked:' || key_id), terminal_cause_code
         FROM request_records WHERE id = $1 AND completed_at IS NOT NULL",
    )
    .bind(request_id)
    .execute(&mut **transaction)
    .await?;
    if inserted.rows_affected() != 1 {
        return Err(AppError::Conflict(
            "terminal projection evidence is missing".into(),
        ));
    }
    Ok(())
}

pub(super) async fn statistics_pruned_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    created_at: i64,
) -> Result<bool, AppError> {
    let pruned: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM observability_prune_boundaries WHERE scope = 'global' AND before_day > $1",
    )
    .bind(created_at / 86_400_000)
    .fetch_one(&mut **transaction)
    .await?;
    Ok(pruned != 0)
}

impl Database {
    pub async fn claim_terminal_projection_tasks(
        &self,
        lease_owner: Uuid,
        limit: i64,
    ) -> Result<Vec<Uuid>, AppError> {
        let now = unix_millis();
        let mut transaction = self.begin_write_transaction().await?;
        let suffix = match self.backend {
            DatabaseBackend::PostgreSql => " FOR UPDATE SKIP LOCKED",
            DatabaseBackend::Sqlite => "",
        };
        let statement = format!(
            "UPDATE terminal_projection_outbox SET lease_owner = $1, lease_expires_at = $2, attempts = attempts + 1 WHERE request_id IN (SELECT request_id FROM terminal_projection_outbox WHERE projected_at IS NULL AND (lease_expires_at IS NULL OR lease_expires_at <= $3) ORDER BY completed_at, request_id LIMIT $4{suffix}) RETURNING request_id"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(lease_owner.to_string())
            .bind(now.saturating_add(300_000))
            .bind(now)
            .bind(limit.clamp(1, 32))
            .fetch_all(&mut *transaction)
            .await?;
        transaction.commit().await?;
        rows.into_iter()
            .map(|row| parse_uuid(row.try_get("request_id")?))
            .collect()
    }

    pub async fn project_claimed_terminal_projection_task(
        &self,
        lease_owner: Uuid,
        request_id: Uuid,
    ) -> Result<bool, AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        lock_request_stats_projection_writer_in_transaction(&mut transaction).await?;
        let now = unix_millis();
        let request_id = request_id.to_string();
        let owner = lease_owner.to_string();
        let claimed = sqlx::query(
            "UPDATE terminal_projection_outbox SET lease_owner = lease_owner WHERE request_id = $1 AND projected_at IS NULL AND lease_owner = $2 AND lease_expires_at > $3 RETURNING tenant_id, key_id, created_at, completed_at",
        )
        .bind(&request_id)
        .bind(&owner)
        .bind(now)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(task) = claimed else {
            transaction.commit().await?;
            return Ok(false);
        };
        let created_at: i64 = task.try_get("created_at")?;
        let pruned = statistics_pruned_in_transaction(&mut transaction, created_at).await?;
        if !pruned {
            let inserted = sqlx::query(
                "INSERT INTO request_stats_facts (request_id, tenant_id, key_id, created_at, model, protocol, status_class, error_code, upstream_account_id, model_route_id, duration_ms, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, generation_units, billing_unit, service_tier, currency, cost_micros, session_id)
                 SELECT request_id, tenant_id, key_id, created_at, model, protocol, CASE WHEN status_code BETWEEN 200 AND 399 AND error_code = '' THEN 'success' ELSE 'failure' END, error_code, upstream_account_id, model_route_id, duration_ms, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, generation_units, billing_unit, service_tier, currency, CASE WHEN (status_code < 200 OR status_code >= 400 OR error_code <> '') AND usage_basis <> 'provider_reported' THEN 0 ELSE cost_micros END, session_id
                 FROM terminal_projection_outbox WHERE request_id = $1 ON CONFLICT(request_id) DO NOTHING",
            )
            .bind(&request_id)
            .execute(&mut *transaction)
            .await?;
            if inserted.rows_affected() != 1 {
                return Err(AppError::Conflict(
                    "unacknowledged terminal projection already has a fact".into(),
                ));
            }
            super::metered_projection::project_metered_request_fact_in_transaction(
                &mut transaction,
                &request_id,
                true,
            )
            .await?;
        }
        let tenant_id: String = task.try_get("tenant_id")?;
        let key_id: String = task.try_get("key_id")?;
        let event = super::lifecycle::allocate_request_event_cursor(
            &mut transaction,
            task.try_get("completed_at")?,
            &tenant_id,
            &key_id,
            &request_id,
        )
        .await?;
        sqlx::query(
            "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code)
             SELECT $1, tenant_id, key_id, request_id, $2, 'finished', protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, NULLIF(error_code, '') FROM terminal_projection_outbox WHERE request_id = $3",
        )
        .bind(&event.event_id)
        .bind(event.event_at)
        .bind(&request_id)
        .execute(&mut *transaction)
        .await?;
        let acknowledged = sqlx::query(
            "UPDATE terminal_projection_outbox SET projected_at = $1, statistics_outcome = $2, lease_owner = NULL, lease_expires_at = NULL WHERE request_id = $3 AND projected_at IS NULL AND lease_owner = $4",
        )
        .bind(now)
        .bind(if pruned { "pruned" } else { "applied" })
        .bind(&request_id)
        .bind(&owner)
        .execute(&mut *transaction)
        .await?;
        if acknowledged.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "terminal projection owner changed".into(),
            ));
        }
        transaction.commit().await?;
        Ok(true)
    }
}
