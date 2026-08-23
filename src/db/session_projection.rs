use sqlx::{Any, Row, Transaction};
use uuid::Uuid;

use super::*;

struct RequestSessionDelta {
    tenant_id: String,
    key_id: String,
    previous_session_id: String,
    authoritative_session_id: String,
    created_at: i64,
    model: String,
    protocol: String,
    status_class: String,
    error_code: String,
    upstream_account_id: String,
    model_route_id: String,
    currency: String,
    duration_ms: i64,
    input_tokens: i64,
    output_tokens: i64,
    cached_input_tokens: i64,
    cache_write_tokens: i64,
    cost_micros: i64,
}

pub(crate) async fn add_request_fact_to_session_projection_in_transaction(
    tx: &mut Transaction<'_, Any>,
    request_id: &str,
) -> Result<(), AppError> {
    sqlx::query(
        r#"INSERT INTO session_usage_totals (
               tenant_id, key_id, session_id, currency, last_activity_at,
               requests, errors, input_tokens, output_tokens, cached_input_tokens,
               cache_write_tokens, generation_units, duration_count,
               duration_sum_ms, cost_micros)
           SELECT tenant_id, key_id, session_id, currency, created_at, 1,
                  CASE WHEN status_class = 'failure' THEN 1 ELSE 0 END,
                  CASE WHEN input_tokens >= cached_input_tokens + cache_write_tokens
                       THEN input_tokens - cached_input_tokens - cache_write_tokens ELSE 0 END,
                  output_tokens, cached_input_tokens, cache_write_tokens, 0,
                  1, duration_ms, cost_micros
             FROM request_stats_facts WHERE request_id = $1
           ON CONFLICT (tenant_id, key_id, session_id, currency) DO UPDATE SET
               last_activity_at = CASE
                   WHEN session_usage_totals.last_activity_at < excluded.last_activity_at
                   THEN excluded.last_activity_at ELSE session_usage_totals.last_activity_at END,
               requests = session_usage_totals.requests + 1,
               errors = session_usage_totals.errors + excluded.errors,
               input_tokens = session_usage_totals.input_tokens + excluded.input_tokens,
               output_tokens = session_usage_totals.output_tokens + excluded.output_tokens,
               cached_input_tokens = session_usage_totals.cached_input_tokens + excluded.cached_input_tokens,
               cache_write_tokens = session_usage_totals.cache_write_tokens + excluded.cache_write_tokens,
               duration_count = session_usage_totals.duration_count + 1,
               duration_sum_ms = session_usage_totals.duration_sum_ms + excluded.duration_sum_ms,
               cost_micros = session_usage_totals.cost_micros + excluded.cost_micros"#,
    )
    .bind(request_id)
    .execute(&mut **tx)
    .await?;

    for (table, bucket_column, divisor) in [
        ("session_usage_hourly", "hour_bucket", 3_600_000_i64),
        ("session_usage_daily", "day_bucket", 86_400_000_i64),
    ] {
        let statement = format!(
            r#"INSERT INTO {table} (
                   tenant_id, key_id, session_id, {bucket_column}, model, protocol,
                   status_class, error_code, upstream_account_id, model_route_id,
                   currency, requests, input_tokens, output_tokens, cached_input_tokens,
                   cache_write_tokens, generation_units, duration_count,
                   duration_sum_ms, cost_micros)
               SELECT tenant_id, key_id, session_id, created_at / {divisor}, model,
                      CASE WHEN protocol = 'anthropic' OR protocol LIKE 'anthropic-%'
                           THEN 'anthropic' WHEN protocol = 'openai-image' THEN 'openai-image'
                           ELSE 'openai' END,
                      status_class, error_code, upstream_account_id, model_route_id,
                      currency, 1,
                      CASE WHEN input_tokens >= cached_input_tokens + cache_write_tokens
                           THEN input_tokens - cached_input_tokens - cache_write_tokens ELSE 0 END,
                      output_tokens, cached_input_tokens, cache_write_tokens, 0,
                      1, duration_ms, cost_micros
                 FROM request_stats_facts WHERE request_id = $1
               ON CONFLICT (
                   tenant_id, key_id, session_id, {bucket_column}, model, protocol,
                   status_class, error_code, upstream_account_id, model_route_id, currency)
               DO UPDATE SET
                   requests = {table}.requests + 1,
                   input_tokens = {table}.input_tokens + excluded.input_tokens,
                   output_tokens = {table}.output_tokens + excluded.output_tokens,
                   cached_input_tokens = {table}.cached_input_tokens + excluded.cached_input_tokens,
                   cache_write_tokens = {table}.cache_write_tokens + excluded.cache_write_tokens,
                   duration_count = {table}.duration_count + 1,
                   duration_sum_ms = {table}.duration_sum_ms + excluded.duration_sum_ms,
                   cost_micros = {table}.cost_micros + excluded.cost_micros"#,
        );
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(request_id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

pub(crate) async fn reclassify_request_session_in_transaction(
    tx: &mut Transaction<'_, Any>,
    request_id: Uuid,
) -> Result<bool, AppError> {
    let request_id = request_id.to_string();
    let row = sqlx::query(
        r#"SELECT fact.tenant_id, fact.key_id, fact.session_id, fact.created_at,
                  fact.model, fact.protocol, fact.status_class, fact.error_code,
                  fact.upstream_account_id, fact.model_route_id, fact.currency,
                  fact.duration_ms, fact.input_tokens, fact.output_tokens,
                  fact.cached_input_tokens, fact.cache_write_tokens,
                  fact.cost_micros,
                  COALESCE(record.conversation_cluster_id,
                           'unlinked:' || fact.key_id) AS authoritative_session_id
             FROM request_stats_facts fact
             JOIN request_records record
               ON record.id = fact.request_id AND record.created_at = fact.created_at
            WHERE fact.request_id = $1"#,
    )
    .bind(&request_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        // Observation-before-finish is normal. The fact insert will use the
        // authoritative request_records cluster later in the same transaction.
        return Ok(false);
    };
    let delta = RequestSessionDelta {
        tenant_id: row.try_get("tenant_id")?,
        key_id: row.try_get("key_id")?,
        previous_session_id: row.try_get("session_id")?,
        authoritative_session_id: row.try_get("authoritative_session_id")?,
        created_at: row.try_get("created_at")?,
        model: row.try_get("model")?,
        protocol: row.try_get("protocol")?,
        status_class: row.try_get("status_class")?,
        error_code: row.try_get("error_code")?,
        upstream_account_id: row.try_get("upstream_account_id")?,
        model_route_id: row.try_get("model_route_id")?,
        currency: row.try_get("currency")?,
        duration_ms: row.try_get("duration_ms")?,
        input_tokens: {
            let input_tokens: i64 = row.try_get("input_tokens")?;
            let cached_input_tokens: i64 = row.try_get("cached_input_tokens")?;
            let cache_write_tokens: i64 = row.try_get("cache_write_tokens")?;
            input_tokens.saturating_sub(cached_input_tokens.saturating_add(cache_write_tokens))
        },
        output_tokens: row.try_get("output_tokens")?,
        cached_input_tokens: row.try_get("cached_input_tokens")?,
        cache_write_tokens: row.try_get("cache_write_tokens")?,
        cost_micros: row.try_get("cost_micros")?,
    };
    if delta.previous_session_id == delta.authoritative_session_id {
        return Ok(false);
    }

    // The conditional update is the membership CAS. PostgreSQL waits on the
    // fact row; SQLite serializes writers. A concurrent/replayed mover therefore
    // cannot rebuild or add the projection twice.
    let moved = sqlx::query(
        "UPDATE request_stats_facts SET session_id = $1 WHERE request_id = $2 AND session_id = $3",
    )
    .bind(&delta.authoritative_session_id)
    .bind(&request_id)
    .bind(&delta.previous_session_id)
    .execute(&mut **tx)
    .await?;
    if moved.rows_affected() == 0 {
        return Ok(false);
    }
    if !delta.previous_session_id.is_empty() {
        remove_request_fact_from_session_projection_in_transaction(tx, &delta).await?;
    }
    add_request_fact_to_session_projection_in_transaction(tx, &request_id).await?;
    Ok(true)
}

async fn remove_request_fact_from_session_projection_in_transaction(
    tx: &mut Transaction<'_, Any>,
    delta: &RequestSessionDelta,
) -> Result<(), AppError> {
    if delta.previous_session_id == format!("unlinked:{}", delta.key_id) {
        return rebuild_request_session_projection_in_transaction(
            tx,
            &delta.tenant_id,
            &delta.key_id,
            &delta.previous_session_id,
        )
        .await;
    }
    let error_delta = i64::from(delta.status_class == "failure");
    let totals = sqlx::query(
        r#"UPDATE session_usage_totals SET
               requests = requests - 1,
               errors = errors - $1,
               input_tokens = input_tokens - $2,
               output_tokens = output_tokens - $3,
               cached_input_tokens = cached_input_tokens - $4,
               cache_write_tokens = cache_write_tokens - $5,
               duration_count = duration_count - 1,
               duration_sum_ms = duration_sum_ms - $6,
               cost_micros = cost_micros - $7
           WHERE tenant_id = $8 AND key_id = $9 AND session_id = $10 AND currency = $11
             AND requests >= 1 AND errors >= $1 AND input_tokens >= $2
             AND output_tokens >= $3 AND cached_input_tokens >= $4
             AND cache_write_tokens >= $5 AND duration_count >= 1
             AND duration_sum_ms >= $6 AND cost_micros >= $7"#,
    )
    .bind(error_delta)
    .bind(delta.input_tokens)
    .bind(delta.output_tokens)
    .bind(delta.cached_input_tokens)
    .bind(delta.cache_write_tokens)
    .bind(delta.duration_ms)
    .bind(delta.cost_micros)
    .bind(&delta.tenant_id)
    .bind(&delta.key_id)
    .bind(&delta.previous_session_id)
    .bind(&delta.currency)
    .execute(&mut **tx)
    .await?;
    let mut rebuild_old = totals.rows_affected() != 1;
    if !rebuild_old {
        let deleted = sqlx::query(
            "DELETE FROM session_usage_totals WHERE tenant_id = $1 AND key_id = $2 AND session_id = $3 AND currency = $4 AND requests = 0",
        )
        .bind(&delta.tenant_id)
        .bind(&delta.key_id)
        .bind(&delta.previous_session_id)
        .bind(&delta.currency)
        .execute(&mut **tx)
        .await?;
        if deleted.rows_affected() == 0 {
            let last_activity_at: i64 = sqlx::query_scalar(
                "SELECT last_activity_at FROM session_usage_totals WHERE tenant_id = $1 AND key_id = $2 AND session_id = $3 AND currency = $4",
            )
            .bind(&delta.tenant_id)
            .bind(&delta.key_id)
            .bind(&delta.previous_session_id)
            .bind(&delta.currency)
            .fetch_one(&mut **tx)
            .await?;
            if last_activity_at == delta.created_at {
                let next_activity: Option<i64> = sqlx::query_scalar(
                    "SELECT MAX(created_at) FROM request_stats_facts WHERE tenant_id = $1 AND key_id = $2 AND session_id = $3 AND currency = $4",
                )
                .bind(&delta.tenant_id)
                .bind(&delta.key_id)
                .bind(&delta.previous_session_id)
                .bind(&delta.currency)
                .fetch_one(&mut **tx)
                .await?;
                let Some(next_activity) = next_activity else {
                    return rebuild_request_session_projection_in_transaction(
                        tx,
                        &delta.tenant_id,
                        &delta.key_id,
                        &delta.previous_session_id,
                    )
                    .await;
                };
                sqlx::query(
                    "UPDATE session_usage_totals SET last_activity_at = $1 WHERE tenant_id = $2 AND key_id = $3 AND session_id = $4 AND currency = $5",
                )
                .bind(next_activity)
                .bind(&delta.tenant_id)
                .bind(&delta.key_id)
                .bind(&delta.previous_session_id)
                .bind(&delta.currency)
                .execute(&mut **tx)
                .await?;
            }
        }
    }

    let canonical_protocol = canonical_session_protocol(&delta.protocol);
    for (table, bucket_column, divisor) in [
        ("session_usage_hourly", "hour_bucket", 3_600_000_i64),
        ("session_usage_daily", "day_bucket", 86_400_000_i64),
    ] {
        let statement = format!(
            r#"UPDATE {table} SET
                   requests = requests - 1,
                   input_tokens = input_tokens - $1,
                   output_tokens = output_tokens - $2,
                   cached_input_tokens = cached_input_tokens - $3,
                   cache_write_tokens = cache_write_tokens - $4,
                   duration_count = duration_count - 1,
                   duration_sum_ms = duration_sum_ms - $5,
                   cost_micros = cost_micros - $6
               WHERE tenant_id = $7 AND key_id = $8 AND session_id = $9
                 AND {bucket_column} = $10 AND model = $11 AND protocol = $12
                 AND status_class = $13 AND error_code = $14
                 AND upstream_account_id = $15 AND model_route_id = $16
                 AND currency = $17 AND requests >= 1 AND input_tokens >= $1
                 AND output_tokens >= $2 AND cached_input_tokens >= $3
                 AND cache_write_tokens >= $4 AND duration_count >= 1
                 AND duration_sum_ms >= $5 AND cost_micros >= $6"#,
        );
        let updated = sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(delta.input_tokens)
            .bind(delta.output_tokens)
            .bind(delta.cached_input_tokens)
            .bind(delta.cache_write_tokens)
            .bind(delta.duration_ms)
            .bind(delta.cost_micros)
            .bind(&delta.tenant_id)
            .bind(&delta.key_id)
            .bind(&delta.previous_session_id)
            .bind(delta.created_at / divisor)
            .bind(&delta.model)
            .bind(canonical_protocol)
            .bind(&delta.status_class)
            .bind(&delta.error_code)
            .bind(&delta.upstream_account_id)
            .bind(&delta.model_route_id)
            .bind(&delta.currency)
            .execute(&mut **tx)
            .await?;
        if updated.rows_affected() != 1 {
            rebuild_old = true;
            continue;
        }
        let delete = format!(
            "DELETE FROM {table} WHERE tenant_id = $1 AND key_id = $2 AND session_id = $3 AND {bucket_column} = $4 AND model = $5 AND protocol = $6 AND status_class = $7 AND error_code = $8 AND upstream_account_id = $9 AND model_route_id = $10 AND currency = $11 AND requests = 0"
        );
        sqlx::query(sqlx::AssertSqlSafe(delete))
            .bind(&delta.tenant_id)
            .bind(&delta.key_id)
            .bind(&delta.previous_session_id)
            .bind(delta.created_at / divisor)
            .bind(&delta.model)
            .bind(canonical_protocol)
            .bind(&delta.status_class)
            .bind(&delta.error_code)
            .bind(&delta.upstream_account_id)
            .bind(&delta.model_route_id)
            .bind(&delta.currency)
            .execute(&mut **tx)
            .await?;
    }
    if rebuild_old {
        rebuild_request_session_projection_in_transaction(
            tx,
            &delta.tenant_id,
            &delta.key_id,
            &delta.previous_session_id,
        )
        .await?;
    }
    Ok(())
}

fn canonical_session_protocol(protocol: &str) -> &str {
    if protocol == "anthropic" || protocol.starts_with("anthropic-") {
        "anthropic"
    } else if protocol == "openai-image" {
        "openai-image"
    } else {
        "openai"
    }
}

pub(crate) async fn add_archive_record_to_session_projection_in_transaction(
    tx: &mut Transaction<'_, Any>,
    tenant_id: Uuid,
    key_id: Uuid,
    source: &str,
    external_request_id: &str,
) -> Result<(), AppError> {
    let inserted = sqlx::query(
        r#"INSERT INTO session_archive_totals (
               tenant_id, key_id, session_id, last_activity_at, requests, errors,
               input_tokens, output_tokens, duration_count, duration_sum_ms)
           SELECT tenant_id, key_id,
                  COALESCE(conversation_cluster_id, 'unlinked:' || key_id),
                  source_started_at, 1,
                  CASE WHEN status_code IS NOT NULL
                             AND (status_code < 200 OR status_code >= 400)
                       THEN 1 ELSE 0 END,
                  input_tokens, output_tokens,
                  CASE WHEN duration_ms IS NULL THEN 0 ELSE 1 END,
                  COALESCE(duration_ms, 0)
             FROM session_archive_unlinked_requests
            WHERE tenant_id = $1 AND key_id = $2 AND source = $3
              AND external_request_id = $4
           ON CONFLICT (tenant_id, key_id, session_id) DO UPDATE SET
               last_activity_at = CASE
                   WHEN session_archive_totals.last_activity_at < excluded.last_activity_at
                   THEN excluded.last_activity_at ELSE session_archive_totals.last_activity_at END,
               requests = session_archive_totals.requests + 1,
               errors = session_archive_totals.errors + excluded.errors,
               input_tokens = session_archive_totals.input_tokens + excluded.input_tokens,
               output_tokens = session_archive_totals.output_tokens + excluded.output_tokens,
               duration_count = session_archive_totals.duration_count + excluded.duration_count,
               duration_sum_ms = session_archive_totals.duration_sum_ms + excluded.duration_sum_ms"#,
    )
    .bind(tenant_id.to_string())
    .bind(key_id.to_string())
    .bind(source)
    .bind(external_request_id)
    .execute(&mut **tx)
    .await?;
    if inserted.rows_affected() != 1 {
        return Err(AppError::Internal);
    }
    Ok(())
}

async fn rebuild_request_session_projection_in_transaction(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    key_id: &str,
    session_id: &str,
) -> Result<(), AppError> {
    for table in [
        "session_usage_totals",
        "session_usage_hourly",
        "session_usage_daily",
    ] {
        let statement =
            format!("DELETE FROM {table} WHERE tenant_id = $1 AND key_id = $2 AND session_id = $3");
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(tenant_id)
            .bind(key_id)
            .bind(session_id)
            .execute(&mut **tx)
            .await?;
    }

    sqlx::query(
        r#"INSERT INTO session_usage_totals (
               tenant_id, key_id, session_id, currency, last_activity_at,
               requests, errors, input_tokens, output_tokens, cached_input_tokens,
               cache_write_tokens, generation_units, duration_count,
               duration_sum_ms, cost_micros)
           SELECT tenant_id, key_id, session_id, currency, MAX(created_at), COUNT(*),
                  SUM(CASE WHEN status_class = 'failure' THEN 1 ELSE 0 END),
                  SUM(CASE
                          WHEN input_tokens >= cached_input_tokens + cache_write_tokens
                          THEN input_tokens - cached_input_tokens - cache_write_tokens ELSE 0 END),
                  SUM(output_tokens), SUM(cached_input_tokens), SUM(cache_write_tokens),
                  0, COUNT(*), SUM(duration_ms), SUM(cost_micros)
             FROM request_stats_facts
            WHERE tenant_id = $1 AND key_id = $2 AND session_id = $3
            GROUP BY tenant_id, key_id, session_id, currency"#,
    )
    .bind(tenant_id)
    .bind(key_id)
    .bind(session_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        r#"INSERT INTO session_usage_totals (
               tenant_id, key_id, session_id, currency, last_activity_at,
               requests, errors, input_tokens, output_tokens, cached_input_tokens,
               cache_write_tokens, generation_units, duration_count,
               duration_sum_ms, cost_micros)
           SELECT tenant_id, key_id, 'unlinked:' || key_id, currency,
                  MAX(created_at), COUNT(*),
                  SUM(CASE WHEN status_class = 'failure' THEN 1 ELSE 0 END),
                  0, 0, 0, 0, SUM(billed_units), COUNT(*), SUM(duration_ms),
                  SUM(cost_micros)
             FROM generation_stats_facts
            WHERE tenant_id = $1 AND key_id = $2
              AND $3 = 'unlinked:' || key_id
            GROUP BY tenant_id, key_id, currency
           ON CONFLICT (tenant_id, key_id, session_id, currency) DO UPDATE SET
               last_activity_at = CASE
                   WHEN session_usage_totals.last_activity_at < excluded.last_activity_at
                   THEN excluded.last_activity_at ELSE session_usage_totals.last_activity_at END,
               requests = session_usage_totals.requests + excluded.requests,
               errors = session_usage_totals.errors + excluded.errors,
               generation_units = session_usage_totals.generation_units + excluded.generation_units,
               duration_count = session_usage_totals.duration_count + excluded.duration_count,
               duration_sum_ms = session_usage_totals.duration_sum_ms + excluded.duration_sum_ms,
               cost_micros = session_usage_totals.cost_micros + excluded.cost_micros"#,
    )
    .bind(tenant_id)
    .bind(key_id)
    .bind(session_id)
    .execute(&mut **tx)
    .await?;
    for (table, bucket_column, divisor) in [
        ("session_usage_hourly", "hour_bucket", 3_600_000_i64),
        ("session_usage_daily", "day_bucket", 86_400_000_i64),
    ] {
        let statement = format!(
            r#"INSERT INTO {table} (
                   tenant_id, key_id, session_id, {bucket_column}, model, protocol,
                   status_class, error_code, upstream_account_id, model_route_id,
                   currency, requests, input_tokens, output_tokens, cached_input_tokens,
                   cache_write_tokens, generation_units, duration_count,
                   duration_sum_ms, cost_micros)
               SELECT tenant_id, key_id, session_id, created_at / {divisor}, model,
                      CASE WHEN protocol = 'anthropic' OR protocol LIKE 'anthropic-%'
                           THEN 'anthropic'
                           WHEN protocol = 'openai-image' THEN 'openai-image'
                           ELSE 'openai' END,
                      status_class, error_code, upstream_account_id, model_route_id,
                      currency, COUNT(*),
                      SUM(CASE
                              WHEN input_tokens >= cached_input_tokens + cache_write_tokens
                              THEN input_tokens - cached_input_tokens - cache_write_tokens ELSE 0 END),
                      SUM(output_tokens), SUM(cached_input_tokens), SUM(cache_write_tokens),
                      0, COUNT(*), SUM(duration_ms), SUM(cost_micros)
                 FROM request_stats_facts
                WHERE tenant_id = $1 AND key_id = $2 AND session_id = $3
                GROUP BY tenant_id, key_id, session_id, created_at / {divisor}, model,
                      CASE WHEN protocol = 'anthropic' OR protocol LIKE 'anthropic-%'
                           THEN 'anthropic'
                           WHEN protocol = 'openai-image' THEN 'openai-image'
                           ELSE 'openai' END,
                      status_class, error_code, upstream_account_id, model_route_id,
                      currency"#
        );
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(tenant_id)
            .bind(key_id)
            .bind(session_id)
            .execute(&mut **tx)
            .await?;
        let generation_statement = format!(
            r#"INSERT INTO {table} (
                   tenant_id, key_id, session_id, {bucket_column}, model, protocol,
                   status_class, error_code, upstream_account_id, model_route_id,
                   currency, requests, input_tokens, output_tokens, cached_input_tokens,
                   cache_write_tokens, generation_units, duration_count,
                   duration_sum_ms, cost_micros)
               SELECT tenant_id, key_id, 'unlinked:' || key_id,
                      created_at / {divisor}, model, 'generation', status_class,
                      error_code, upstream_account_id, model_route_id, currency,
                      COUNT(*), 0, 0, 0, 0, SUM(billed_units), COUNT(*),
                      SUM(duration_ms), SUM(cost_micros)
                 FROM generation_stats_facts
                WHERE tenant_id = $1 AND key_id = $2
                  AND $3 = 'unlinked:' || key_id
                GROUP BY tenant_id, key_id, created_at / {divisor}, model,
                         status_class, error_code, upstream_account_id,
                         model_route_id, currency"#,
        );
        sqlx::query(sqlx::AssertSqlSafe(generation_statement))
            .bind(tenant_id)
            .bind(key_id)
            .bind(session_id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}
