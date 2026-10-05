use super::super::*;

pub(super) async fn enqueue_terminal_projection_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    request_id: &str,
) -> Result<(), AppError> {
    let inserted = sqlx::query(
        "INSERT INTO terminal_projection_outbox (request_id, reservation_id, tenant_id, key_id, account_id, created_at, completed_at, model, protocol, status_code, error_code, upstream_account_id, model_route_id, duration_ms, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, generation_units, billing_unit, service_tier, currency, cost_micros, usage_basis, session_id, terminal_cause_code, response_object, account_projected_at)
         SELECT r.id, r.reservation_id, r.tenant_id, r.key_id, u.account_id, r.created_at, r.completed_at, r.model, r.protocol, r.status_code, COALESCE(r.error_code, ''), COALESCE(r.upstream_account_id, ''), COALESCE(r.model_route_id, ''), COALESCE(r.duration_ms, 0), r.input_tokens, r.output_tokens, r.cached_input_tokens, r.cache_write_tokens, r.billed_units, r.billing_unit, r.service_tier, r.currency, r.cost_micros, COALESCE(r.usage_basis, ''), COALESCE(r.conversation_cluster_id, 'unlinked:' || r.key_id), r.terminal_cause_code, r.response_object, CASE WHEN u.enforcement_mode = 'prepaid' THEN r.completed_at ELSE m.projected_at END
         FROM request_records r JOIN usage_reservations u ON u.id = r.reservation_id LEFT JOIN metered_usage_projection_outbox m ON m.reservation_id = u.id WHERE r.id = $1 AND r.completed_at IS NOT NULL",
    )
    .bind(request_id)
    .execute(&mut **transaction)
    .await?;
    if inserted.rows_affected() != 1 {
        return Err(AppError::Conflict(
            "terminal projection evidence is missing".into(),
        ));
    }
    sqlx::query("UPDATE metered_usage_projection_outbox SET lease_owner = 'terminal-v118', lease_expires_at = 9223372036854775807 WHERE reservation_id IN (SELECT reservation_id FROM terminal_projection_outbox WHERE request_id = $1) AND projected_at IS NULL")
        .bind(request_id).execute(&mut **transaction).await?;
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
            "UPDATE terminal_projection_outbox SET lease_owner = $1, lease_expires_at = $2, attempts = attempts + 1 WHERE request_id IN (SELECT request_id FROM terminal_projection_outbox WHERE projected_at IS NULL AND (lease_owner IS NULL OR lease_owner <> $1) AND (lease_expires_at IS NULL OR lease_expires_at <= $3) ORDER BY completed_at, request_id LIMIT $4{suffix}) RETURNING request_id"
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
        if !self
            .project_terminal_account_receipt(lease_owner, request_id)
            .await?
        {
            return Ok(false);
        }
        let mut transaction = self.begin_write_transaction().await?;
        lock_request_stats_projection_writer_in_transaction(&mut transaction).await?;
        let now = unix_millis();
        let request_id = request_id.to_string();
        let owner = lease_owner.to_string();
        let claimed = sqlx::query(
            "UPDATE terminal_projection_outbox SET lease_owner = lease_owner WHERE request_id = $1 AND projected_at IS NULL AND lease_owner = $2 AND lease_expires_at > $3 RETURNING tenant_id, key_id, account_id, reservation_id, cost_micros, account_projected_at, created_at, completed_at",
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
            "UPDATE terminal_projection_outbox SET projected_at = $1, statistics_outcome = $2, lease_owner = NULL, lease_expires_at = NULL WHERE request_id = $3 AND projected_at IS NULL AND lease_owner = $4 AND account_projected_at IS NOT NULL",
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

    async fn project_terminal_account_receipt(
        &self,
        lease_owner: Uuid,
        request_id: Uuid,
    ) -> Result<bool, AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        let now = unix_millis();
        let task = sqlx::query("UPDATE terminal_projection_outbox SET lease_owner = lease_owner WHERE request_id = $1 AND projected_at IS NULL AND lease_owner = $2 AND lease_expires_at > $3 RETURNING reservation_id, account_id, key_id, cost_micros, account_projected_at")
            .bind(request_id.to_string()).bind(lease_owner.to_string()).bind(now).fetch_optional(&mut *transaction).await?;
        let Some(task) = task else {
            return Ok(false);
        };
        if task
            .try_get::<Option<i64>, _>("account_projected_at")?
            .is_none()
        {
            let reservation_id: String = task.try_get("reservation_id")?;
            let account_id: String = task.try_get("account_id")?;
            let key_id: String = task.try_get("key_id")?;
            let cost_micros: i64 = task.try_get("cost_micros")?;
            let matched: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM metered_usage_projection_outbox WHERE reservation_id = $1 AND account_id = $2 AND key_id = $3 AND actual_micros = $4 AND projected_at IS NULL AND lease_owner = 'terminal-v118'")
                .bind(&reservation_id).bind(&account_id).bind(&key_id).bind(cost_micros).fetch_one(&mut *transaction).await?;
            if matched != 1 {
                return Err(AppError::Conflict(
                    "terminal account receipt does not match its settlement".into(),
                ));
            }
            super::super::billing::project_account_usage_in_transaction(
                &mut transaction,
                &account_id,
                cost_micros,
                now,
            )
            .await?;
            let acknowledged = sqlx::query("UPDATE metered_usage_projection_outbox SET projected_at = $1, lease_owner = NULL, lease_expires_at = NULL WHERE reservation_id = $2 AND projected_at IS NULL AND lease_owner = 'terminal-v118'")
                .bind(now).bind(&reservation_id).execute(&mut *transaction).await?;
            if acknowledged.rows_affected() != 1 {
                return Err(AppError::Conflict(
                    "terminal account receipt ownership changed".into(),
                ));
            }
            sqlx::query("UPDATE terminal_projection_outbox SET account_projected_at = $1 WHERE request_id = $2")
                .bind(now).bind(request_id.to_string()).execute(&mut *transaction).await?;
        }
        transaction.commit().await?;
        Ok(true)
    }
}
