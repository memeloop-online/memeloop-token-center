use super::super::*;

pub(super) async fn enqueue_unknown_terminal_projection_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    request_id: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO terminal_projection_outbox (request_id, reservation_id, tenant_id, key_id, account_id, created_at, completed_at, model, protocol, status_code, error_code, upstream_account_id, model_route_id, duration_ms, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, cache_known_read_tokens, cache_known_input_tokens, generation_units, billing_unit, service_tier, currency, cost_micros, usage_basis, session_id, terminal_cause_code, response_object, account_projected_at)
         SELECT r.id, p.reservation_id, p.tenant_id, p.key_id, p.account_id, r.created_at, o.completed_at, r.model, r.protocol, o.status_code, COALESCE(o.error_code, ''), COALESCE(o.upstream_account_id, ''), COALESCE(o.model_route_id, ''), o.duration_ms, 0, 0, 0, 0, NULL, NULL, 0, '', r.service_tier, r.currency, 0, 'not_observed', COALESCE(r.conversation_cluster_id, 'unlinked:' || r.key_id), o.terminal_cause_code, o.response_object, o.completed_at
         FROM pending_proxy_settlements p
         JOIN request_records r ON r.id = p.request_id AND r.tenant_id = p.tenant_id AND r.key_id = p.key_id AND r.reservation_id = p.reservation_id
         JOIN proxy_operational_terminals o ON o.request_id = p.request_id AND o.request_created_at = r.created_at AND o.tenant_id = p.tenant_id AND o.key_id = p.key_id AND o.account_id = p.account_id AND o.reservation_id = p.reservation_id AND o.owner_token = p.owner_token AND o.fence = p.fence AND o.contract_digest = p.contract_digest
         WHERE p.request_id = $1 AND p.state = 'pending' AND p.unknown_reason IS NOT NULL AND r.completed_at IS NULL
         ON CONFLICT(request_id) DO NOTHING",
    )
    .bind(request_id)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

pub(super) async fn enqueue_terminal_projection_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    request_id: &str,
) -> Result<(), AppError> {
    let inserted = sqlx::query(
        "INSERT INTO terminal_projection_outbox (request_id, reservation_id, tenant_id, key_id, account_id, created_at, completed_at, model, protocol, status_code, error_code, upstream_account_id, model_route_id, duration_ms, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, cache_known_read_tokens, cache_known_input_tokens, generation_units, billing_unit, service_tier, currency, cost_micros, usage_basis, session_id, terminal_cause_code, response_object, account_projected_at)
         SELECT r.id, r.reservation_id, r.tenant_id, r.key_id, u.account_id, r.created_at, COALESCE(o.completed_at, r.completed_at), r.model, r.protocol, COALESCE(o.status_code, r.status_code), COALESCE(o.error_code, r.error_code, ''), COALESCE(o.upstream_account_id, r.upstream_account_id, ''), COALESCE(o.model_route_id, r.model_route_id, ''), COALESCE(o.duration_ms, r.duration_ms, 0), r.input_tokens, r.output_tokens, r.cached_input_tokens, r.cache_write_tokens, r.cache_known_read_tokens, r.cache_known_input_tokens, r.billed_units, r.billing_unit, r.service_tier, r.currency, r.cost_micros, COALESCE(r.usage_basis, ''), COALESCE(r.conversation_cluster_id, 'unlinked:' || r.key_id), COALESCE(o.terminal_cause_code, r.terminal_cause_code), COALESCE(o.response_object, r.response_object), CASE WHEN u.enforcement_mode = 'prepaid' THEN r.completed_at ELSE m.projected_at END
         FROM request_records r JOIN usage_reservations u ON u.id = r.reservation_id LEFT JOIN metered_usage_projection_outbox m ON m.reservation_id = u.id LEFT JOIN proxy_operational_terminals o ON o.request_id = r.id AND o.request_created_at = r.created_at AND o.tenant_id = r.tenant_id AND o.key_id = r.key_id AND o.reservation_id = r.reservation_id
         WHERE r.id = $1 AND r.completed_at IS NOT NULL
         ON CONFLICT(request_id) DO UPDATE SET completed_at = excluded.completed_at, status_code = excluded.status_code, error_code = excluded.error_code, upstream_account_id = excluded.upstream_account_id, model_route_id = excluded.model_route_id, duration_ms = excluded.duration_ms, input_tokens = excluded.input_tokens, output_tokens = excluded.output_tokens, cached_input_tokens = excluded.cached_input_tokens, cache_write_tokens = excluded.cache_write_tokens, cache_known_read_tokens = excluded.cache_known_read_tokens, cache_known_input_tokens = excluded.cache_known_input_tokens, generation_units = excluded.generation_units, billing_unit = excluded.billing_unit, service_tier = excluded.service_tier, cost_micros = excluded.cost_micros, usage_basis = excluded.usage_basis, session_id = excluded.session_id, terminal_cause_code = excluded.terminal_cause_code, response_object = excluded.response_object, account_projected_at = excluded.account_projected_at, projected_at = NULL, statistics_outcome = NULL, lease_owner = NULL, lease_expires_at = NULL
         WHERE terminal_projection_outbox.tenant_id = excluded.tenant_id AND terminal_projection_outbox.key_id = excluded.key_id AND terminal_projection_outbox.account_id = excluded.account_id AND terminal_projection_outbox.reservation_id = excluded.reservation_id AND terminal_projection_outbox.currency = excluded.currency AND terminal_projection_outbox.usage_basis = 'not_observed'
           AND EXISTS (SELECT 1 FROM pending_proxy_settlements p WHERE p.request_id = excluded.request_id AND p.state = 'pending' AND p.unknown_reason IS NOT NULL)",
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
        let missing: Vec<String> = sqlx::query_scalar("SELECT p.request_id FROM pending_proxy_settlements p JOIN request_records r ON r.id = p.request_id AND r.tenant_id = p.tenant_id AND r.key_id = p.key_id AND r.reservation_id = p.reservation_id JOIN proxy_operational_terminals o ON o.request_id = p.request_id AND o.request_created_at = r.created_at AND o.tenant_id = p.tenant_id AND o.key_id = p.key_id AND o.account_id = p.account_id AND o.reservation_id = p.reservation_id AND o.owner_token = p.owner_token AND o.fence = p.fence AND o.contract_digest = p.contract_digest WHERE p.state = 'pending' AND p.unknown_reason IS NOT NULL AND r.completed_at IS NULL AND NOT EXISTS (SELECT 1 FROM terminal_projection_outbox t WHERE t.request_id = p.request_id) ORDER BY p.updated_at, p.request_id LIMIT $1")
            .bind(limit.clamp(1, 32)).fetch_all(&mut *transaction).await?;
        for request_id in missing {
            sqlx::query("UPDATE request_records SET completed_at = completed_at WHERE id = $1 AND completed_at IS NULL")
                .bind(&request_id).execute(&mut *transaction).await?;
            enqueue_unknown_terminal_projection_in_transaction(&mut transaction, &request_id)
                .await?;
        }
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
        sqlx::query(
            "UPDATE request_stats_facts SET cost_micros = cost_micros WHERE request_id = $1",
        )
        .bind(&request_id)
        .execute(&mut *transaction)
        .await?;
        let previous = sqlx::query("SELECT f.tenant_id, f.key_id, f.currency, f.session_id FROM request_stats_facts f JOIN pending_proxy_settlements p ON p.request_id = f.request_id AND p.tenant_id = f.tenant_id AND p.key_id = f.key_id WHERE f.request_id = $1 AND p.unknown_reason IS NOT NULL AND f.input_tokens = 0 AND f.output_tokens = 0 AND f.cached_input_tokens = 0 AND f.cache_write_tokens = 0 AND f.cost_micros = 0")
            .bind(&request_id).fetch_optional(&mut *transaction).await?;
        let fact_exists: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM request_stats_facts WHERE request_id = $1")
                .bind(&request_id)
                .fetch_one(&mut *transaction)
                .await?;
        if fact_exists != i64::from(previous.is_some()) {
            return Err(AppError::Conflict(
                "unacknowledged terminal projection already has a fact".into(),
            ));
        }
        if !pruned {
            if previous.is_some() {
                super::metered_projection::project_metered_request_fact_in_transaction(
                    &mut transaction,
                    &request_id,
                    true,
                    -1,
                )
                .await?;
            }
            let inserted = sqlx::query(
                "INSERT INTO request_stats_facts (request_id, tenant_id, key_id, created_at, model, protocol, status_class, error_code, upstream_account_id, model_route_id, duration_ms, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, cache_known_read_tokens, cache_known_input_tokens, generation_units, billing_unit, service_tier, currency, cost_micros, session_id)
                 SELECT request_id, tenant_id, key_id, created_at, model, protocol, CASE WHEN status_code BETWEEN 200 AND 399 AND error_code = '' THEN 'success' ELSE 'failure' END, error_code, upstream_account_id, model_route_id, duration_ms, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, cache_known_read_tokens, cache_known_input_tokens, generation_units, billing_unit, service_tier, currency, CASE WHEN (status_code < 200 OR status_code >= 400 OR error_code <> '') AND usage_basis <> 'provider_reported' THEN 0 ELSE cost_micros END, session_id
                 FROM terminal_projection_outbox WHERE request_id = $1 ON CONFLICT(request_id) DO UPDATE SET status_class = excluded.status_class, error_code = excluded.error_code, upstream_account_id = excluded.upstream_account_id, model_route_id = excluded.model_route_id, duration_ms = excluded.duration_ms, input_tokens = excluded.input_tokens, output_tokens = excluded.output_tokens, cached_input_tokens = excluded.cached_input_tokens, cache_write_tokens = excluded.cache_write_tokens, cache_known_read_tokens = excluded.cache_known_read_tokens, cache_known_input_tokens = excluded.cache_known_input_tokens, generation_units = excluded.generation_units, billing_unit = excluded.billing_unit, service_tier = excluded.service_tier, cost_micros = excluded.cost_micros
                 WHERE request_stats_facts.tenant_id = excluded.tenant_id AND request_stats_facts.key_id = excluded.key_id AND request_stats_facts.currency = excluded.currency AND EXISTS (SELECT 1 FROM pending_proxy_settlements p WHERE p.request_id = excluded.request_id AND p.unknown_reason IS NOT NULL)",
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
                1,
            )
            .await?;
        }
        let tenant_id: String = task.try_get("tenant_id")?;
        let key_id: String = task.try_get("key_id")?;
        let already_finished: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM request_events WHERE request_id = $1 AND event_kind = 'finished'",
        )
        .bind(&request_id)
        .fetch_one(&mut *transaction)
        .await?;
        if already_finished == 0 {
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
        }
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
