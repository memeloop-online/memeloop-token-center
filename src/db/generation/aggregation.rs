use sqlx::{Any, Transaction};

use crate::error::AppError;

/// Adds a terminal generation job to the daily aggregate exactly once.
///
/// The marker and aggregate update deliberately share the caller's transaction with the terminal
/// state change. A retry therefore observes either both writes or neither write.
pub(super) async fn aggregate_terminal_generation_job(
    transaction: &mut Transaction<'_, Any>,
    job_id: &str,
    now: i64,
) -> Result<(), AppError> {
    let claimed = sqlx::query(
        "UPDATE generation_jobs SET stats_aggregated_at = $1 WHERE id = $2 AND stats_aggregated_at IS NULL AND status IN ('succeeded', 'failed', 'cancelled')",
    )
    .bind(now)
    .bind(job_id)
    .execute(&mut **transaction)
    .await?;
    if claimed.rows_affected() == 0 {
        return Ok(());
    }

    let fact = sqlx::query(
        r#"INSERT INTO generation_stats_facts (
               job_id, tenant_id, key_id, created_at, model, status_class,
               error_code, upstream_account_id, duration_ms, cost_micros,
               billed_units, currency, modality, billing_unit, model_route_id)
           SELECT j.id, j.tenant_id, j.key_id, j.created_at, j.public_model,
                  CASE WHEN j.status = 'succeeded' THEN 'success' ELSE 'failure' END,
                  COALESCE(j.error_code, ''), COALESCE(j.upstream_account_id, ''),
                  CASE WHEN j.completed_at IS NULL OR j.completed_at < j.created_at
                       THEN 0 ELSE j.completed_at - j.created_at END,
                  j.cost_micros, COALESCE(j.billed_units, 0), k.currency,
                  CASE
                      WHEN EXISTS (
                          SELECT 1 FROM generation_assets asset
                           WHERE asset.job_id = j.id AND asset.mime_type LIKE 'video/%'
                      ) THEN 'video'
                      WHEN EXISTS (
                          SELECT 1 FROM generation_assets asset
                           WHERE asset.job_id = j.id AND asset.mime_type LIKE 'image/%'
                      ) THEN 'image'
                      WHEN j.driver = 'volcengine-seedance' THEN 'video'
                      ELSE 'unknown'
                  END,
                  COALESCE(NULLIF(j.billing_unit_snapshot, ''), 'unknown'),
                  COALESCE(j.model_route_id, '')
             FROM generation_jobs j
             JOIN key_records k ON k.id = j.key_id AND k.tenant_id = j.tenant_id
            WHERE j.id = $1 AND j.status IN ('succeeded', 'failed', 'cancelled')
           ON CONFLICT (job_id) DO NOTHING"#,
    )
    .bind(job_id)
    .execute(&mut **transaction)
    .await?;
    if fact.rows_affected() != 1 {
        return Err(AppError::Internal);
    }

    let aggregated = sqlx::query(
        "INSERT INTO generation_daily_aggregates (tenant_id, key_id, day_bucket, model, status_class, error_code, upstream_account_id, requests, billed_units, cost_micros, currency) SELECT f.tenant_id, f.key_id, f.created_at / 86400000, f.model, f.status_class, f.error_code, f.upstream_account_id, 1, f.billed_units, f.cost_micros, f.currency FROM generation_stats_facts f WHERE f.job_id = $1 ON CONFLICT (tenant_id, key_id, day_bucket, model, status_class, error_code, upstream_account_id, currency) DO UPDATE SET requests = generation_daily_aggregates.requests + excluded.requests, billed_units = generation_daily_aggregates.billed_units + excluded.billed_units, cost_micros = generation_daily_aggregates.cost_micros + excluded.cost_micros",
    )
    .bind(job_id)
    .execute(&mut **transaction)
    .await?;
    if aggregated.rows_affected() != 1 {
        return Err(AppError::Internal);
    }
    sqlx::query(
        r#"INSERT INTO usage_analysis_hourly (
               tenant_id, key_id, hour_bucket, source_kind, model, protocol, status_class,
               error_code, upstream_account_id, model_route_id, service_tier, currency,
               requests, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens,
               generation_units, duration_count, duration_sum_ms, duration_bucket_0,
               duration_bucket_1, duration_bucket_2, duration_bucket_3, duration_bucket_4,
               duration_bucket_5, duration_bucket_6, duration_bucket_7, duration_bucket_8,
               duration_bucket_9, duration_bucket_10, duration_bucket_11, cost_micros)
           SELECT g.tenant_id, g.key_id, g.created_at / 3600000, 'generation', g.model,
                  'generation', g.status_class, g.error_code, g.upstream_account_id, g.model_route_id,
                  'default', g.currency, 1, 0, 0, 0, 0, g.billed_units, 1, g.duration_ms,
                  CASE WHEN g.duration_ms <= 10 THEN 1 ELSE 0 END,
                  CASE WHEN g.duration_ms > 10 AND g.duration_ms <= 50 THEN 1 ELSE 0 END,
                  CASE WHEN g.duration_ms > 50 AND g.duration_ms <= 100 THEN 1 ELSE 0 END,
                  CASE WHEN g.duration_ms > 100 AND g.duration_ms <= 250 THEN 1 ELSE 0 END,
                  CASE WHEN g.duration_ms > 250 AND g.duration_ms <= 500 THEN 1 ELSE 0 END,
                  CASE WHEN g.duration_ms > 500 AND g.duration_ms <= 1000 THEN 1 ELSE 0 END,
                  CASE WHEN g.duration_ms > 1000 AND g.duration_ms <= 2500 THEN 1 ELSE 0 END,
                  CASE WHEN g.duration_ms > 2500 AND g.duration_ms <= 5000 THEN 1 ELSE 0 END,
                  CASE WHEN g.duration_ms > 5000 AND g.duration_ms <= 10000 THEN 1 ELSE 0 END,
                  CASE WHEN g.duration_ms > 10000 AND g.duration_ms <= 30000 THEN 1 ELSE 0 END,
                  CASE WHEN g.duration_ms > 30000 AND g.duration_ms <= 60000 THEN 1 ELSE 0 END,
                  CASE WHEN g.duration_ms > 60000 THEN 1 ELSE 0 END,
                  g.cost_micros
             FROM generation_stats_facts g
            WHERE g.job_id = $1
           ON CONFLICT (tenant_id, key_id, hour_bucket, source_kind, model, protocol,
                        status_class, error_code, upstream_account_id, model_route_id,
                        service_tier, currency)
           DO UPDATE SET requests = usage_analysis_hourly.requests + excluded.requests,
               generation_units = usage_analysis_hourly.generation_units + excluded.generation_units,
               duration_count = usage_analysis_hourly.duration_count + excluded.duration_count,
               duration_sum_ms = usage_analysis_hourly.duration_sum_ms + excluded.duration_sum_ms,
               duration_bucket_0 = usage_analysis_hourly.duration_bucket_0 + excluded.duration_bucket_0,
               duration_bucket_1 = usage_analysis_hourly.duration_bucket_1 + excluded.duration_bucket_1,
               duration_bucket_2 = usage_analysis_hourly.duration_bucket_2 + excluded.duration_bucket_2,
               duration_bucket_3 = usage_analysis_hourly.duration_bucket_3 + excluded.duration_bucket_3,
               duration_bucket_4 = usage_analysis_hourly.duration_bucket_4 + excluded.duration_bucket_4,
               duration_bucket_5 = usage_analysis_hourly.duration_bucket_5 + excluded.duration_bucket_5,
               duration_bucket_6 = usage_analysis_hourly.duration_bucket_6 + excluded.duration_bucket_6,
               duration_bucket_7 = usage_analysis_hourly.duration_bucket_7 + excluded.duration_bucket_7,
               duration_bucket_8 = usage_analysis_hourly.duration_bucket_8 + excluded.duration_bucket_8,
               duration_bucket_9 = usage_analysis_hourly.duration_bucket_9 + excluded.duration_bucket_9,
               duration_bucket_10 = usage_analysis_hourly.duration_bucket_10 + excluded.duration_bucket_10,
               duration_bucket_11 = usage_analysis_hourly.duration_bucket_11 + excluded.duration_bucket_11,
               cost_micros = usage_analysis_hourly.cost_micros + excluded.cost_micros"#,
    )
    .bind(job_id)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        r#"INSERT INTO usage_analysis_daily (
               tenant_id, key_id, day_bucket, source_kind, model, protocol, status_class,
               error_code, upstream_account_id, model_route_id, service_tier, currency,
               requests, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens,
               generation_units, duration_count, duration_sum_ms, duration_bucket_0,
               duration_bucket_1, duration_bucket_2, duration_bucket_3, duration_bucket_4,
               duration_bucket_5, duration_bucket_6, duration_bucket_7, duration_bucket_8,
               duration_bucket_9, duration_bucket_10, duration_bucket_11, cost_micros)
           SELECT g.tenant_id, g.key_id, g.created_at / 86400000, 'generation', g.model,
                  'generation', g.status_class, g.error_code, g.upstream_account_id, g.model_route_id,
                  'default', g.currency, 1, 0, 0, 0, 0, g.billed_units, 1, g.duration_ms,
                  CASE WHEN g.duration_ms <= 10 THEN 1 ELSE 0 END,
                  CASE WHEN g.duration_ms > 10 AND g.duration_ms <= 50 THEN 1 ELSE 0 END,
                  CASE WHEN g.duration_ms > 50 AND g.duration_ms <= 100 THEN 1 ELSE 0 END,
                  CASE WHEN g.duration_ms > 100 AND g.duration_ms <= 250 THEN 1 ELSE 0 END,
                  CASE WHEN g.duration_ms > 250 AND g.duration_ms <= 500 THEN 1 ELSE 0 END,
                  CASE WHEN g.duration_ms > 500 AND g.duration_ms <= 1000 THEN 1 ELSE 0 END,
                  CASE WHEN g.duration_ms > 1000 AND g.duration_ms <= 2500 THEN 1 ELSE 0 END,
                  CASE WHEN g.duration_ms > 2500 AND g.duration_ms <= 5000 THEN 1 ELSE 0 END,
                  CASE WHEN g.duration_ms > 5000 AND g.duration_ms <= 10000 THEN 1 ELSE 0 END,
                  CASE WHEN g.duration_ms > 10000 AND g.duration_ms <= 30000 THEN 1 ELSE 0 END,
                  CASE WHEN g.duration_ms > 30000 AND g.duration_ms <= 60000 THEN 1 ELSE 0 END,
                  CASE WHEN g.duration_ms > 60000 THEN 1 ELSE 0 END,
                  g.cost_micros
             FROM generation_stats_facts g
            WHERE g.job_id = $1
           ON CONFLICT (tenant_id, key_id, day_bucket, source_kind, model, protocol,
                        status_class, error_code, upstream_account_id, model_route_id,
                        service_tier, currency)
           DO UPDATE SET requests = usage_analysis_daily.requests + excluded.requests,
               generation_units = usage_analysis_daily.generation_units + excluded.generation_units,
               duration_count = usage_analysis_daily.duration_count + excluded.duration_count,
               duration_sum_ms = usage_analysis_daily.duration_sum_ms + excluded.duration_sum_ms,
               duration_bucket_0 = usage_analysis_daily.duration_bucket_0 + excluded.duration_bucket_0,
               duration_bucket_1 = usage_analysis_daily.duration_bucket_1 + excluded.duration_bucket_1,
               duration_bucket_2 = usage_analysis_daily.duration_bucket_2 + excluded.duration_bucket_2,
               duration_bucket_3 = usage_analysis_daily.duration_bucket_3 + excluded.duration_bucket_3,
               duration_bucket_4 = usage_analysis_daily.duration_bucket_4 + excluded.duration_bucket_4,
               duration_bucket_5 = usage_analysis_daily.duration_bucket_5 + excluded.duration_bucket_5,
               duration_bucket_6 = usage_analysis_daily.duration_bucket_6 + excluded.duration_bucket_6,
               duration_bucket_7 = usage_analysis_daily.duration_bucket_7 + excluded.duration_bucket_7,
               duration_bucket_8 = usage_analysis_daily.duration_bucket_8 + excluded.duration_bucket_8,
               duration_bucket_9 = usage_analysis_daily.duration_bucket_9 + excluded.duration_bucket_9,
               duration_bucket_10 = usage_analysis_daily.duration_bucket_10 + excluded.duration_bucket_10,
               duration_bucket_11 = usage_analysis_daily.duration_bucket_11 + excluded.duration_bucket_11,
               cost_micros = usage_analysis_daily.cost_micros + excluded.cost_micros"#,
    )
    .bind(job_id)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        r#"INSERT INTO generation_usage_dimensions_hourly (
               tenant_id, key_id, hour_bucket, model, status_class, error_code,
               upstream_account_id, model_route_id, modality, billing_unit,
               currency, units)
           SELECT tenant_id, key_id, created_at / 3600000, model, status_class,
                  error_code, upstream_account_id, model_route_id, modality,
                  billing_unit, currency, billed_units
             FROM generation_stats_facts
            WHERE job_id = $1
           ON CONFLICT (
               tenant_id, key_id, hour_bucket, model, status_class, error_code,
               upstream_account_id, model_route_id, modality, billing_unit, currency)
           DO UPDATE SET units = generation_usage_dimensions_hourly.units + excluded.units"#,
    )
    .bind(job_id)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        r#"INSERT INTO generation_usage_dimensions_daily (
               tenant_id, key_id, day_bucket, model, status_class, error_code,
               upstream_account_id, model_route_id, modality, billing_unit,
               currency, units)
           SELECT tenant_id, key_id, created_at / 86400000, model, status_class,
                  error_code, upstream_account_id, model_route_id, modality,
                  billing_unit, currency, billed_units
             FROM generation_stats_facts
            WHERE job_id = $1
           ON CONFLICT (
               tenant_id, key_id, day_bucket, model, status_class, error_code,
               upstream_account_id, model_route_id, modality, billing_unit, currency)
           DO UPDATE SET units = generation_usage_dimensions_daily.units + excluded.units"#,
    )
    .bind(job_id)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}
