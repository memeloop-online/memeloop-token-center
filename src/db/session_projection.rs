use sqlx::{Any, Row, Transaction};
use uuid::Uuid;

use super::*;

pub(crate) async fn add_request_fact_to_session_projection_in_transaction(
    tx: &mut Transaction<'_, Any>,
    request_id: &str,
) -> Result<(), AppError> {
    sqlx::query(
        r#"INSERT INTO session_usage_totals (
               tenant_id, key_id, session_id, currency, last_activity_at,
               requests, errors, input_tokens, output_tokens, duration_count,
               duration_sum_ms, cost_micros)
           SELECT tenant_id, key_id, session_id, currency, created_at, 1,
                  CASE WHEN status_class = 'failure' THEN 1 ELSE 0 END,
                  input_tokens, output_tokens, 1, duration_ms, cost_micros
             FROM request_stats_facts WHERE request_id = $1
           ON CONFLICT (tenant_id, key_id, session_id, currency) DO UPDATE SET
               last_activity_at = CASE
                   WHEN session_usage_totals.last_activity_at < excluded.last_activity_at
                   THEN excluded.last_activity_at ELSE session_usage_totals.last_activity_at END,
               requests = session_usage_totals.requests + 1,
               errors = session_usage_totals.errors + excluded.errors,
               input_tokens = session_usage_totals.input_tokens + excluded.input_tokens,
               output_tokens = session_usage_totals.output_tokens + excluded.output_tokens,
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
                   currency, requests, input_tokens, output_tokens, duration_count,
                   duration_sum_ms, cost_micros)
               SELECT tenant_id, key_id, session_id, created_at / {divisor}, model,
                      CASE WHEN protocol = 'anthropic' OR protocol LIKE 'anthropic-%'
                           THEN 'anthropic' WHEN protocol = 'openai-image' THEN 'openai-image'
                           ELSE 'openai' END,
                      status_class, error_code, upstream_account_id, model_route_id,
                      currency, 1, input_tokens, output_tokens, 1, duration_ms, cost_micros
                 FROM request_stats_facts WHERE request_id = $1
               ON CONFLICT (
                   tenant_id, key_id, session_id, {bucket_column}, model, protocol,
                   status_class, error_code, upstream_account_id, model_route_id, currency)
               DO UPDATE SET
                   requests = {table}.requests + 1,
                   input_tokens = {table}.input_tokens + excluded.input_tokens,
                   output_tokens = {table}.output_tokens + excluded.output_tokens,
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
        r#"SELECT fact.tenant_id, fact.key_id, fact.session_id,
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
    let tenant_id: String = row.try_get("tenant_id")?;
    let key_id: String = row.try_get("key_id")?;
    let previous_session_id: String = row.try_get("session_id")?;
    let authoritative_session_id: String = row.try_get("authoritative_session_id")?;
    if previous_session_id == authoritative_session_id {
        return Ok(false);
    }

    // The conditional update is the membership CAS. PostgreSQL waits on the
    // fact row; SQLite serializes writers. A concurrent/replayed mover therefore
    // cannot rebuild or add the projection twice.
    let moved = sqlx::query(
        "UPDATE request_stats_facts SET session_id = $1 WHERE request_id = $2 AND session_id = $3",
    )
    .bind(&authoritative_session_id)
    .bind(&request_id)
    .bind(&previous_session_id)
    .execute(&mut **tx)
    .await?;
    if moved.rows_affected() == 0 {
        return Ok(false);
    }
    if !previous_session_id.is_empty() {
        rebuild_request_session_projection_in_transaction(
            tx,
            &tenant_id,
            &key_id,
            &previous_session_id,
        )
        .await?;
    }
    rebuild_request_session_projection_in_transaction(
        tx,
        &tenant_id,
        &key_id,
        &authoritative_session_id,
    )
    .await?;
    Ok(true)
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
               requests, errors, input_tokens, output_tokens, duration_count,
               duration_sum_ms, cost_micros)
           SELECT tenant_id, key_id, session_id, currency, MAX(created_at), COUNT(*),
                  SUM(CASE WHEN status_class = 'failure' THEN 1 ELSE 0 END),
                  SUM(input_tokens), SUM(output_tokens), COUNT(*), SUM(duration_ms),
                  SUM(cost_micros)
             FROM request_stats_facts
            WHERE tenant_id = $1 AND key_id = $2 AND session_id = $3
            GROUP BY tenant_id, key_id, session_id, currency"#,
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
                   currency, requests, input_tokens, output_tokens, duration_count,
                   duration_sum_ms, cost_micros)
               SELECT tenant_id, key_id, session_id, created_at / {divisor}, model,
                      CASE WHEN protocol = 'anthropic' OR protocol LIKE 'anthropic-%'
                           THEN 'anthropic'
                           WHEN protocol = 'openai-image' THEN 'openai-image'
                           ELSE 'openai' END,
                      status_class, error_code, upstream_account_id, model_route_id,
                      currency, COUNT(*), SUM(input_tokens), SUM(output_tokens),
                      COUNT(*), SUM(duration_ms), SUM(cost_micros)
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
    }
    Ok(())
}

pub(crate) async fn rebuild_archive_session_projection_in_transaction(
    tx: &mut Transaction<'_, Any>,
    tenant_id: Uuid,
    key_id: Uuid,
    session_id: &str,
) -> Result<(), AppError> {
    let tenant_id = tenant_id.to_string();
    let key_id = key_id.to_string();
    sqlx::query(
        "DELETE FROM session_archive_totals WHERE tenant_id = $1 AND key_id = $2 AND session_id = $3",
    )
    .bind(&tenant_id)
    .bind(&key_id)
    .bind(session_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        r#"INSERT INTO session_archive_totals (
               tenant_id, key_id, session_id, last_activity_at, requests, errors,
               input_tokens, output_tokens, duration_count, duration_sum_ms)
           SELECT tenant_id, key_id,
                  COALESCE(conversation_cluster_id, 'unlinked:' || key_id),
                  MAX(source_started_at), COUNT(*),
                  SUM(CASE WHEN status_code IS NOT NULL
                                AND (status_code < 200 OR status_code >= 400)
                           THEN 1 ELSE 0 END),
                  SUM(input_tokens), SUM(output_tokens),
                  SUM(CASE WHEN duration_ms IS NULL THEN 0 ELSE 1 END),
                  SUM(COALESCE(duration_ms, 0))
             FROM session_archive_unlinked_requests
            WHERE tenant_id = $1 AND key_id = $2
              AND COALESCE(conversation_cluster_id, 'unlinked:' || key_id) = $3
            GROUP BY tenant_id, key_id,
                     COALESCE(conversation_cluster_id, 'unlinked:' || key_id)"#,
    )
    .bind(&tenant_id)
    .bind(&key_id)
    .bind(session_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
