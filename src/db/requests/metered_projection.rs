use uuid::Uuid;

use super::super::*;

/// A lease-owned metered settlement awaiting its read-model projections.
///
/// The settlement itself has already committed. Projectors receive only the
/// durable reservation identity, so they cannot alter the charged amount or
/// the original request dimensions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeteredUsageProjectionTask {
    pub reservation_id: Uuid,
}

const METERED_USAGE_PROJECTION_BATCH_LIMIT: i64 = 32;
const METERED_USAGE_PROJECTION_LEASE_MILLIS: i64 = 5 * 60 * 1_000;

impl Database {
    /// Claims a bounded set of unprojected metered settlements.
    ///
    /// PostgreSQL claimers use row locks with `SKIP LOCKED`. SQLite's immediate
    /// write transaction serializes the same selector. A worker crash leaves a
    /// lease that expires into another exactly-once projection attempt.
    pub async fn claim_metered_usage_projection_tasks(
        &self,
        lease_owner: Uuid,
        limit: i64,
    ) -> Result<Vec<MeteredUsageProjectionTask>, AppError> {
        let now = unix_millis();
        let expires_at = now.saturating_add(METERED_USAGE_PROJECTION_LEASE_MILLIS);
        let limit = limit.clamp(1, METERED_USAGE_PROJECTION_BATCH_LIMIT);
        let mut transaction = self.begin_write_transaction().await?;
        let claimable = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT reservation_id FROM metered_usage_projection_outbox WHERE projected_at IS NULL AND (lease_expires_at IS NULL OR lease_expires_at <= $3) ORDER BY created_at ASC, reservation_id ASC LIMIT $4 FOR UPDATE SKIP LOCKED"
            }
            // SQLite's BEGIN IMMEDIATE writer transaction serializes claimers,
            // so this selector is race-free without a row-lock clause.
            DatabaseBackend::Sqlite => {
                "SELECT reservation_id FROM metered_usage_projection_outbox WHERE projected_at IS NULL AND (lease_expires_at IS NULL OR lease_expires_at <= $3) ORDER BY created_at ASC, reservation_id ASC LIMIT $4"
            }
        };
        let statement = format!(
            "UPDATE metered_usage_projection_outbox SET lease_owner = $1, lease_expires_at = $2, attempts = attempts + 1 WHERE reservation_id IN ({claimable}) RETURNING reservation_id"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(lease_owner.to_string())
            .bind(expires_at)
            .bind(now)
            .bind(limit)
            .fetch_all(&mut *transaction)
            .await?;
        transaction.commit().await?;
        rows.into_iter()
            .map(|row| {
                Ok(MeteredUsageProjectionTask {
                    reservation_id: parse_uuid(row.try_get("reservation_id")?)?,
                })
            })
            .collect()
    }

    /// Projects one claimed settlement and acknowledges its outbox row in the
    /// same transaction. It intentionally writes only append-style usage
    /// read models; balances, prepaid budget state, and admission windows are
    /// never touched by this path.
    pub async fn project_claimed_metered_usage_projection_task(
        &self,
        lease_owner: Uuid,
        reservation_id: Uuid,
    ) -> Result<bool, AppError> {
        let now = unix_millis();
        let reservation_id = reservation_id.to_string();
        let mut transaction = self.begin_write_transaction().await?;
        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT account_id, key_id, actual_micros FROM metered_usage_projection_outbox WHERE reservation_id = $1 AND projected_at IS NULL AND lease_owner = $2 AND lease_expires_at >= $3 FOR UPDATE"
            }
            DatabaseBackend::Sqlite => {
                "SELECT account_id, key_id, actual_micros FROM metered_usage_projection_outbox WHERE reservation_id = $1 AND projected_at IS NULL AND lease_owner = $2 AND lease_expires_at >= $3"
            }
        };
        let task = sqlx::query(select)
            .bind(&reservation_id)
            .bind(lease_owner.to_string())
            .bind(now)
            .fetch_optional(&mut *transaction)
            .await?;
        let Some(task) = task else {
            transaction.commit().await?;
            return Ok(false);
        };
        let account_id: String = task.try_get("account_id")?;
        let key_id: String = task.try_get("key_id")?;
        let actual_micros: i64 = task.try_get("actual_micros")?;

        // Verify the immutable settlement source before acknowledging it. A
        // corrupt or partially repaired row remains pending for investigation
        // instead of being silently discarded.
        let source_matches: i64 = sqlx::query(
            "SELECT COUNT(*) AS matched_rows FROM usage_reservations WHERE id = $1 AND account_id = $2 AND key_id = $3 AND actual_micros = $4 AND status = 'settled' AND enforcement_mode = 'metered_unlimited'",
        )
        .bind(&reservation_id)
        .bind(&account_id)
        .bind(&key_id)
        .bind(actual_micros)
        .fetch_one(&mut *transaction)
        .await?
        .try_get("matched_rows")?;
        if source_matches != 1 {
            return Err(AppError::Conflict(
                "metered usage projection source no longer matches its settlement".into(),
            ));
        }

        // Token requests have exactly one request fact. Asynchronous generation
        // jobs have no request row and already own their generation projections,
        // so they only need the durable acknowledgement below.
        let request_ids = sqlx::query(
            "SELECT id FROM request_records WHERE reservation_id = $1 AND key_id = $2 ORDER BY id ASC LIMIT 2",
        )
        .bind(&reservation_id)
        .bind(&key_id)
        .fetch_all(&mut *transaction)
        .await?;
        if request_ids.len() > 1 {
            return Err(AppError::Conflict(
                "metered settlement is attached to more than one request".into(),
            ));
        }
        if let Some(row) = request_ids.into_iter().next() {
            let request_id: String = row.try_get("id")?;
            let fact_cost: Option<i64> = sqlx::query(
                "SELECT cost_micros FROM request_stats_facts WHERE request_id = $1",
            )
            .bind(&request_id)
            .fetch_optional(&mut *transaction)
            .await?
            .map(|row| row.try_get("cost_micros"))
            .transpose()?;
            if fact_cost != Some(actual_micros) {
                return Err(AppError::Conflict(
                    "metered request fact is missing or does not match its settlement".into(),
                ));
            }
            // Conversation projection reclassifies its fact and builds the
            // session rollups atomically. Avoid adding the same fact here when
            // that durable task exists: separate worker processes may claim
            // the two outboxes in either order.
            let deferred_conversation: i64 = sqlx::query(
                "SELECT COUNT(*) AS queued FROM conversation_projection_outbox WHERE request_id = $1",
            )
            .bind(&request_id)
            .fetch_one(&mut *transaction)
            .await?
            .try_get("queued")?;
            project_metered_request_fact_in_transaction(
                &mut transaction,
                &request_id,
                deferred_conversation == 0,
            )
            .await?;
        }

        let acknowledged = sqlx::query(
            "UPDATE metered_usage_projection_outbox SET projected_at = $1, lease_owner = NULL, lease_expires_at = NULL WHERE reservation_id = $2 AND projected_at IS NULL AND lease_owner = $3",
        )
        .bind(now)
        .bind(&reservation_id)
        .bind(lease_owner.to_string())
        .execute(&mut *transaction)
        .await?;
        if acknowledged.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "metered usage projection lease ownership changed".into(),
            ));
        }
        transaction.commit().await?;
        Ok(true)
    }
}

async fn project_metered_request_fact_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    request_id: &str,
    project_session_rollups: bool,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO usage_daily_aggregates (key_id, day_bucket, model, status_class, error_code, requests, input_tokens, output_tokens, cost_micros) SELECT key_id, created_at / 86400000, model, status_class, error_code, 1, input_tokens, output_tokens, cost_micros FROM request_stats_facts WHERE request_id = $1 ON CONFLICT(key_id, day_bucket, model, status_class, error_code) DO UPDATE SET requests = usage_daily_aggregates.requests + excluded.requests, input_tokens = usage_daily_aggregates.input_tokens + excluded.input_tokens, output_tokens = usage_daily_aggregates.output_tokens + excluded.output_tokens, cost_micros = usage_daily_aggregates.cost_micros + excluded.cost_micros",
    )
    .bind(request_id)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO request_daily_aggregates (tenant_id, key_id, day_bucket, model, protocol, status_class, error_code, upstream_account_id, model_route_id, service_tier, currency, requests, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, duration_count, duration_sum_ms, cost_micros) SELECT tenant_id, key_id, created_at / 86400000, model, protocol, status_class, error_code, upstream_account_id, model_route_id, service_tier, currency, 1, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, 1, duration_ms, cost_micros FROM request_stats_facts WHERE request_id = $1 ON CONFLICT(tenant_id, key_id, day_bucket, model, protocol, status_class, error_code, upstream_account_id, model_route_id, service_tier, currency) DO UPDATE SET requests = request_daily_aggregates.requests + excluded.requests, input_tokens = request_daily_aggregates.input_tokens + excluded.input_tokens, output_tokens = request_daily_aggregates.output_tokens + excluded.output_tokens, cached_input_tokens = request_daily_aggregates.cached_input_tokens + excluded.cached_input_tokens, cache_write_tokens = request_daily_aggregates.cache_write_tokens + excluded.cache_write_tokens, duration_count = request_daily_aggregates.duration_count + excluded.duration_count, duration_sum_ms = request_daily_aggregates.duration_sum_ms + excluded.duration_sum_ms, cost_micros = request_daily_aggregates.cost_micros + excluded.cost_micros",
    )
    .bind(request_id)
    .execute(&mut **transaction)
    .await?;
    if project_session_rollups {
        add_request_fact_to_session_projection_in_transaction(transaction, request_id).await?;
    }
    project_metered_request_analysis_in_transaction(transaction, request_id, "usage_analysis_hourly", "hour_bucket", 3_600_000).await?;
    project_metered_request_analysis_in_transaction(transaction, request_id, "usage_analysis_daily", "day_bucket", 86_400_000).await?;
    Ok(())
}

async fn project_metered_request_analysis_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    request_id: &str,
    table: &str,
    bucket_column: &str,
    divisor: i64,
) -> Result<(), AppError> {
    let statement = format!(
        r#"INSERT INTO {table} (
               tenant_id, key_id, {bucket_column}, source_kind, model, protocol, status_class,
               error_code, upstream_account_id, model_route_id, service_tier, currency,
               requests, input_tokens, output_tokens, cached_input_tokens,
               cache_write_tokens, generation_units, duration_count, duration_sum_ms,
               duration_bucket_0, duration_bucket_1, duration_bucket_2, duration_bucket_3,
               duration_bucket_4, duration_bucket_5, duration_bucket_6, duration_bucket_7,
               duration_bucket_8, duration_bucket_9, duration_bucket_10,
               duration_bucket_11, cost_micros)
           SELECT tenant_id, key_id, created_at / {divisor}, 'request', model,
                  CASE WHEN protocol = 'anthropic' OR protocol LIKE 'anthropic-%'
                       THEN 'anthropic' WHEN protocol = 'openai-image' THEN 'openai-image'
                       ELSE 'openai' END,
                  status_class, error_code, upstream_account_id, model_route_id,
                  service_tier, currency, 1,
                  CASE WHEN input_tokens >= cached_input_tokens + cache_write_tokens
                       THEN input_tokens - cached_input_tokens - cache_write_tokens ELSE 0 END,
                  output_tokens, cached_input_tokens, cache_write_tokens, 0, 1, duration_ms,
                  CASE WHEN duration_ms <= 10 THEN 1 ELSE 0 END,
                  CASE WHEN duration_ms > 10 AND duration_ms <= 50 THEN 1 ELSE 0 END,
                  CASE WHEN duration_ms > 50 AND duration_ms <= 100 THEN 1 ELSE 0 END,
                  CASE WHEN duration_ms > 100 AND duration_ms <= 250 THEN 1 ELSE 0 END,
                  CASE WHEN duration_ms > 250 AND duration_ms <= 500 THEN 1 ELSE 0 END,
                  CASE WHEN duration_ms > 500 AND duration_ms <= 1000 THEN 1 ELSE 0 END,
                  CASE WHEN duration_ms > 1000 AND duration_ms <= 2500 THEN 1 ELSE 0 END,
                  CASE WHEN duration_ms > 2500 AND duration_ms <= 5000 THEN 1 ELSE 0 END,
                  CASE WHEN duration_ms > 5000 AND duration_ms <= 10000 THEN 1 ELSE 0 END,
                  CASE WHEN duration_ms > 10000 AND duration_ms <= 30000 THEN 1 ELSE 0 END,
                  CASE WHEN duration_ms > 30000 AND duration_ms <= 60000 THEN 1 ELSE 0 END,
                  CASE WHEN duration_ms > 60000 THEN 1 ELSE 0 END,
                  cost_micros
             FROM request_stats_facts WHERE request_id = $1
           ON CONFLICT (tenant_id, key_id, {bucket_column}, source_kind, model, protocol,
                        status_class, error_code, upstream_account_id, model_route_id,
                        service_tier, currency)
           DO UPDATE SET requests = {table}.requests + excluded.requests,
               input_tokens = {table}.input_tokens + excluded.input_tokens,
               output_tokens = {table}.output_tokens + excluded.output_tokens,
               cached_input_tokens = {table}.cached_input_tokens + excluded.cached_input_tokens,
               cache_write_tokens = {table}.cache_write_tokens + excluded.cache_write_tokens,
               generation_units = {table}.generation_units + excluded.generation_units,
               duration_count = {table}.duration_count + excluded.duration_count,
               duration_sum_ms = {table}.duration_sum_ms + excluded.duration_sum_ms,
               duration_bucket_0 = {table}.duration_bucket_0 + excluded.duration_bucket_0,
               duration_bucket_1 = {table}.duration_bucket_1 + excluded.duration_bucket_1,
               duration_bucket_2 = {table}.duration_bucket_2 + excluded.duration_bucket_2,
               duration_bucket_3 = {table}.duration_bucket_3 + excluded.duration_bucket_3,
               duration_bucket_4 = {table}.duration_bucket_4 + excluded.duration_bucket_4,
               duration_bucket_5 = {table}.duration_bucket_5 + excluded.duration_bucket_5,
               duration_bucket_6 = {table}.duration_bucket_6 + excluded.duration_bucket_6,
               duration_bucket_7 = {table}.duration_bucket_7 + excluded.duration_bucket_7,
               duration_bucket_8 = {table}.duration_bucket_8 + excluded.duration_bucket_8,
               duration_bucket_9 = {table}.duration_bucket_9 + excluded.duration_bucket_9,
               duration_bucket_10 = {table}.duration_bucket_10 + excluded.duration_bucket_10,
               duration_bucket_11 = {table}.duration_bucket_11 + excluded.duration_bucket_11,
               cost_micros = {table}.cost_micros + excluded.cost_micros"#,
    );
    sqlx::query(sqlx::AssertSqlSafe(statement))
        .bind(request_id)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}
