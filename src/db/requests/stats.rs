use super::super::*;

pub(crate) const MAX_STATS_RANGE_MILLIS: i64 = 93 * 86_400_000;

pub(crate) const FILTERED_ACTIVITY_SOURCE_ROLLUPS: &str = r#"
SELECT a.day_bucket * 86400000 AS created_at,
       a.model,
       a.protocol,
       a.status_class,
       a.error_code,
       a.input_tokens,
       a.output_tokens,
       a.cost_micros,
       a.requests
FROM request_daily_aggregates a
JOIN key_records k ON k.id = a.key_id AND k.tenant_id = a.tenant_id
JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id
JOIN tenants t ON t.id = a.tenant_id
WHERE ($1 = '' OR t.external_id = $1)
  AND ($2 = '' OR a.key_id = $2)
  AND a.day_bucket >= $17 / 86400000
  AND a.day_bucket < $18 / 86400000
  AND ($5 = '' OR a.model = $5)
  AND ($6 = '' OR a.protocol = $6)
  AND ($7 = ''
       OR ($7 = 'success' AND a.status_class = 'success')
       OR ($7 = 'error' AND a.status_class = 'failure'))
  AND ($8 = '' OR a.error_code = $8)
  AND ($9 = '' OR a.upstream_account_id = $9)
  AND ($10 = '' OR a.model_route_id = $10)
  AND $11 < 0 AND $12 < 0 AND $13 < 0 AND $14 < 0
  AND ($15 = '' OR LOWER(k.alias) LIKE $15 ESCAPE '\')
  AND ($16 = '' OR LOWER(p.external_id) LIKE $16 ESCAPE '\')
UNION ALL
SELECT f.created_at,
       f.model,
       f.protocol,
       f.status_class,
       f.error_code,
       f.input_tokens,
       f.output_tokens,
       f.cost_micros,
       CAST(1 AS BIGINT) AS requests
FROM request_stats_facts f
JOIN key_records k ON k.id = f.key_id AND k.tenant_id = f.tenant_id
JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id
JOIN tenants t ON t.id = f.tenant_id
WHERE ($1 = '' OR t.external_id = $1)
  AND ($2 = '' OR f.key_id = $2)
  AND f.created_at >= $3 AND f.created_at <= $4
  AND (f.created_at < $17 OR f.created_at >= $18)
  AND ($5 = '' OR f.model = $5)
  AND ($6 = '' OR f.protocol = $6)
  AND ($7 = ''
       OR ($7 = 'success' AND f.status_class = 'success')
       OR ($7 = 'error' AND f.status_class = 'failure'))
  AND ($8 = '' OR f.error_code = $8)
  AND ($9 = '' OR f.upstream_account_id = $9)
  AND ($10 = '' OR f.model_route_id = $10)
  AND $11 < 0 AND $12 < 0 AND $13 < 0 AND $14 < 0
  AND ($15 = '' OR LOWER(k.alias) LIKE $15 ESCAPE '\')
  AND ($16 = '' OR LOWER(p.external_id) LIKE $16 ESCAPE '\')
UNION ALL
SELECT a.day_bucket * 86400000 AS created_at,
       a.model,
       'generation' AS protocol,
       a.status_class,
       a.error_code,
       0 AS input_tokens,
       0 AS output_tokens,
       a.cost_micros,
       a.requests
FROM generation_daily_aggregates a
JOIN key_records k ON k.id = a.key_id AND k.tenant_id = a.tenant_id
JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id
JOIN tenants t ON t.id = a.tenant_id
WHERE ($1 = '' OR t.external_id = $1)
  AND ($2 = '' OR a.key_id = $2)
  AND a.day_bucket >= $17 / 86400000
  AND a.day_bucket < $18 / 86400000
  AND ($5 = '' OR a.model = $5)
  AND ($6 = '' OR $6 = 'generation')
  AND ($7 = ''
       OR ($7 = 'success' AND a.status_class = 'success')
       OR ($7 = 'error' AND a.status_class = 'failure'))
  AND ($8 = '' OR a.error_code = $8)
  AND ($9 = '' OR a.upstream_account_id = $9)
  AND $10 = ''
  AND $11 < 0 AND $12 < 0 AND $13 < 0 AND $14 < 0
  AND ($15 = '' OR LOWER(k.alias) LIKE $15 ESCAPE '\')
  AND ($16 = '' OR LOWER(p.external_id) LIKE $16 ESCAPE '\')
UNION ALL
SELECT f.created_at,
       f.model,
       'generation' AS protocol,
       f.status_class,
       f.error_code,
       0 AS input_tokens,
       0 AS output_tokens,
       f.cost_micros,
       CAST(1 AS BIGINT) AS requests
FROM generation_stats_facts f
JOIN key_records k ON k.id = f.key_id AND k.tenant_id = f.tenant_id
JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id
JOIN tenants t ON t.id = f.tenant_id
WHERE ($1 = '' OR t.external_id = $1)
  AND ($2 = '' OR f.key_id = $2)
  AND f.created_at >= $3 AND f.created_at <= $4
  AND (f.created_at < $17 OR f.created_at >= $18)
  AND ($5 = '' OR f.model = $5)
  AND ($6 = '' OR $6 = 'generation')
  AND ($7 = ''
       OR ($7 = 'success' AND f.status_class = 'success')
       OR ($7 = 'error' AND f.status_class = 'failure'))
  AND ($8 = '' OR f.error_code = $8)
  AND ($9 = '' OR f.upstream_account_id = $9)
  AND $10 = ''
  AND $11 < 0 AND $12 < 0 AND $13 < 0 AND $14 < 0
  AND ($15 = '' OR LOWER(k.alias) LIKE $15 ESCAPE '\')
  AND ($16 = '' OR LOWER(p.external_id) LIKE $16 ESCAPE '\')
"#;

pub(crate) const FILTERED_ACTIVITY_SOURCE_FACTS: &str = r#"
SELECT f.created_at,
       f.model,
       f.protocol,
       f.status_class,
       f.error_code,
       f.input_tokens,
       f.output_tokens,
       f.cost_micros,
       CAST(1 AS BIGINT) AS requests
FROM request_stats_facts f
JOIN key_records k ON k.id = f.key_id AND k.tenant_id = f.tenant_id
JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id
JOIN tenants t ON t.id = f.tenant_id
WHERE ($1 = '' OR t.external_id = $1)
  AND ($2 = '' OR f.key_id = $2)
  AND f.created_at >= $3 AND f.created_at <= $4
  AND ($5 = '' OR f.model = $5)
  AND ($6 = '' OR f.protocol = $6)
  AND ($7 = ''
       OR ($7 = 'success' AND f.status_class = 'success')
       OR ($7 = 'error' AND f.status_class = 'failure'))
  AND ($8 = '' OR f.error_code = $8)
  AND ($9 = '' OR f.upstream_account_id = $9)
  AND ($10 = '' OR f.model_route_id = $10)
  AND ($11 < 0 OR f.duration_ms >= $11)
  AND ($12 < 0 OR f.duration_ms <= $12)
  AND ($13 < 0 OR f.cost_micros >= $13)
  AND ($14 < 0 OR f.cost_micros <= $14)
  AND ($15 = '' OR LOWER(k.alias) LIKE $15 ESCAPE '\')
  AND ($16 = '' OR LOWER(p.external_id) LIKE $16 ESCAPE '\')
  AND $17 >= 0 AND $18 >= 0
UNION ALL
SELECT f.created_at,
       f.model,
       'generation' AS protocol,
       f.status_class,
       f.error_code,
       0 AS input_tokens,
       0 AS output_tokens,
       f.cost_micros,
       CAST(1 AS BIGINT) AS requests
FROM generation_stats_facts f
JOIN key_records k ON k.id = f.key_id AND k.tenant_id = f.tenant_id
JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id
JOIN tenants t ON t.id = f.tenant_id
WHERE ($1 = '' OR t.external_id = $1)
  AND ($2 = '' OR f.key_id = $2)
  AND f.created_at >= $3 AND f.created_at <= $4
  AND ($5 = '' OR f.model = $5)
  AND ($6 = '' OR $6 = 'generation')
  AND ($7 = ''
       OR ($7 = 'success' AND f.status_class = 'success')
       OR ($7 = 'error' AND f.status_class = 'failure'))
  AND ($8 = '' OR f.error_code = $8)
  AND ($9 = '' OR f.upstream_account_id = $9)
  AND $10 = ''
  AND ($11 < 0 OR f.duration_ms >= $11)
  AND ($12 < 0 OR f.duration_ms <= $12)
  AND ($13 < 0 OR f.cost_micros >= $13)
  AND ($14 < 0 OR f.cost_micros <= $14)
  AND ($15 = '' OR LOWER(k.alias) LIKE $15 ESCAPE '\')
  AND ($16 = '' OR LOWER(p.external_id) LIKE $16 ESCAPE '\')
  AND $17 >= 0 AND $18 >= 0
"#;

pub(crate) const FILTERED_ACTIVITY_SOURCE_PENDING: &str = r#"
SELECT r.created_at,
       r.model,
       r.protocol,
       CASE WHEN r.status_code BETWEEN 200 AND 399 THEN 'success'
            WHEN r.status_code IS NULL THEN 'pending' ELSE 'failure' END AS status_class,
       COALESCE(r.error_code, '') AS error_code,
       r.input_tokens,
       r.output_tokens,
       r.cost_micros,
       CAST(1 AS BIGINT) AS requests
FROM request_records r
JOIN key_records k ON k.id = r.key_id AND k.tenant_id = r.tenant_id
JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id
JOIN tenants t ON t.id = r.tenant_id
WHERE ($1 = '' OR t.external_id = $1)
  AND ($2 = '' OR r.key_id = $2)
  AND r.created_at >= $3 AND r.created_at <= $4
  AND ($5 = '' OR r.model = $5)
  AND ($6 = '' OR r.protocol = $6)
  AND $7 = 'pending'
  AND r.status_code IS NULL
  AND ($8 = '' OR r.error_code = $8)
  AND ($9 = '' OR r.upstream_account_id = $9)
  AND ($10 = '' OR r.model_route_id = $10)
  AND ($11 < 0 OR r.duration_ms >= $11)
  AND ($12 < 0 OR r.duration_ms <= $12)
  AND ($13 < 0 OR r.cost_micros >= $13)
  AND ($14 < 0 OR r.cost_micros <= $14)
  AND ($15 = '' OR LOWER(k.alias) LIKE $15 ESCAPE '\')
  AND ($16 = '' OR LOWER(p.external_id) LIKE $16 ESCAPE '\')
UNION ALL
SELECT g.created_at,
       g.public_model AS model,
       'generation' AS protocol,
       'pending' AS status_class,
       COALESCE(g.error_code, '') AS error_code,
       0 AS input_tokens,
       0 AS output_tokens,
       g.cost_micros,
       CAST(1 AS BIGINT) AS requests
FROM generation_jobs g
JOIN key_records k ON k.id = g.key_id AND k.tenant_id = g.tenant_id
JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id
JOIN tenants t ON t.id = g.tenant_id
WHERE ($1 = '' OR t.external_id = $1)
  AND ($2 = '' OR g.key_id = $2)
  AND g.created_at >= $3 AND g.created_at <= $4
  AND g.status IN ('preparing', 'queued', 'submitting', 'running')
  AND ($5 = '' OR g.public_model = $5)
  AND ($6 = '' OR $6 = 'generation')
  AND $7 = 'pending'
  AND ($8 = '' OR g.error_code = $8)
  AND ($9 = '' OR g.upstream_account_id = $9)
  AND $10 = ''
  AND $11 < 0
  AND $12 < 0
  AND ($13 < 0 OR g.cost_micros >= $13)
  AND ($14 < 0 OR g.cost_micros <= $14)
  AND ($15 = '' OR LOWER(k.alias) LIKE $15 ESCAPE '\')
  AND ($16 = '' OR LOWER(p.external_id) LIKE $16 ESCAPE '\')
  AND $17 >= 0 AND $18 >= 0
"#;

#[derive(Clone, Debug, Default)]
pub struct StatsFilter {
    pub from_created_at: Option<i64>,
    pub to_created_at: Option<i64>,
    pub key_id: Option<Uuid>,
    pub model: Option<String>,
    pub protocol: Option<String>,
    pub status: Option<String>,
    pub error_code: Option<String>,
    pub upstream_account_id: Option<Uuid>,
    pub route_id: Option<Uuid>,
    pub min_duration_ms: Option<i64>,
    pub max_duration_ms: Option<i64>,
    pub min_cost_micros: Option<i64>,
    pub max_cost_micros: Option<i64>,
    pub key_alias: Option<String>,
    pub principal: Option<String>,
}

impl Database {
    pub async fn stats_filtered(
        &self,
        key_id: Uuid,
        mut filter: StatsFilter,
    ) -> Result<SelfStats, AppError> {
        // A downstream credential can never widen its view by supplying a different key_id.
        filter.key_id = Some(key_id);
        let stats = self.aggregate_filtered_stats(None, &filter).await?;
        Ok(SelfStats {
            key_id,
            summary: stats.summary,
            by_model: stats.by_model,
            by_day: stats.by_day,
            errors: stats.errors,
        })
    }

    pub async fn operator_stats_filtered(
        &self,
        tenant_external_id: &str,
        filter: StatsFilter,
    ) -> Result<OperatorStats, AppError> {
        self.aggregate_filtered_stats(Some(tenant_external_id), &filter)
            .await
    }

    pub async fn global_operator_stats_filtered(
        &self,
        filter: StatsFilter,
    ) -> Result<OperatorStats, AppError> {
        self.aggregate_filtered_stats(None, &filter).await
    }

    async fn aggregate_filtered_stats(
        &self,
        tenant_external_id: Option<&str>,
        filter: &StatsFilter,
    ) -> Result<OperatorStats, AppError> {
        validate_stats_filter(filter)?;
        let tenant_external_id = tenant_external_id.unwrap_or_default();
        let key_id = filter.key_id.map(|id| id.to_string()).unwrap_or_default();
        let from_created_at = filter.from_created_at.ok_or_else(|| {
            AppError::BadRequest("from_created_at is required for statistics".into())
        })?;
        let to_created_at = filter.to_created_at.ok_or_else(|| {
            AppError::BadRequest("to_created_at is required for statistics".into())
        })?;
        let upstream_account_id = filter
            .upstream_account_id
            .map(|id| id.to_string())
            .unwrap_or_default();
        let route_id = filter.route_id.map(|id| id.to_string()).unwrap_or_default();
        let key_alias = search_prefix(filter.key_alias.as_deref());
        let principal = search_prefix(filter.principal.as_deref());
        // Ordinary terminal statistics use compact daily rollups for complete UTC days and
        // exact facts for the two possible boundary days. Per-request duration/cost filters
        // use compact facts for the whole bounded interval. Only transient pending requests
        // need the raw request/job tables.
        let use_facts = filter.min_duration_ms.is_some()
            || filter.max_duration_ms.is_some()
            || filter.min_cost_micros.is_some()
            || filter.max_cost_micros.is_some();
        let activity_source = if filter.status.as_deref() == Some("pending") {
            FILTERED_ACTIVITY_SOURCE_PENDING
        } else if use_facts {
            FILTERED_ACTIVITY_SOURCE_FACTS
        } else {
            FILTERED_ACTIVITY_SOURCE_ROLLUPS
        };
        const DAY_MILLIS: i64 = 86_400_000;
        let full_day_from = from_created_at
            .saturating_add(DAY_MILLIS - 1)
            .div_euclid(DAY_MILLIS)
            .saturating_mul(DAY_MILLIS);
        let full_day_to_exclusive = to_created_at
            .saturating_add(1)
            .div_euclid(DAY_MILLIS)
            .saturating_mul(DAY_MILLIS);

        macro_rules! bind_activity_filter {
            ($query:expr) => {
                $query
                    .bind(tenant_external_id)
                    .bind(&key_id)
                    .bind(from_created_at)
                    .bind(to_created_at)
                    .bind(filter.model.as_deref().unwrap_or_default())
                    .bind(filter.protocol.as_deref().unwrap_or_default())
                    .bind(filter.status.as_deref().unwrap_or_default())
                    .bind(filter.error_code.as_deref().unwrap_or_default())
                    .bind(&upstream_account_id)
                    .bind(&route_id)
                    .bind(filter.min_duration_ms.unwrap_or(-1))
                    .bind(filter.max_duration_ms.unwrap_or(-1))
                    .bind(filter.min_cost_micros.unwrap_or(-1))
                    .bind(filter.max_cost_micros.unwrap_or(-1))
                    .bind(&key_alias)
                    .bind(&principal)
                    .bind(full_day_from)
                    .bind(full_day_to_exclusive)
            };
        }

        // PostgreSQL GROUPING SETS computes all four response projections while reading the
        // selected rollup/fact source once. SQLite lacks GROUPING SETS, so its single statement
        // materializes that source once before the four bounded projections.
        let stats_sql = match self.backend {
            DatabaseBackend::PostgreSql => format!(
                r#"
WITH filtered_activity AS MATERIALIZED ({activity_source}),
enriched AS (
    SELECT model,
           created_at / 86400000 AS day_bucket,
           NULLIF(error_code, '') AS error_bucket,
           status_class,
           requests,
           input_tokens,
           output_tokens,
           cost_micros
      FROM filtered_activity
),
grouped AS (
    SELECT CASE
               WHEN GROUPING(model) = 0 THEN 'model'
               WHEN GROUPING(day_bucket) = 0 THEN 'day'
               WHEN GROUPING(error_bucket) = 0 THEN 'error'
               ELSE 'summary'
           END AS bucket_kind,
           CASE
               WHEN GROUPING(model) = 0 THEN model
               WHEN GROUPING(error_bucket) = 0 THEN error_bucket
               ELSE ''
           END AS name,
           CASE WHEN GROUPING(day_bucket) = 0 THEN day_bucket ELSE -1 END AS day_bucket,
           CAST(COALESCE(SUM(requests), 0) AS BIGINT) AS requests,
           CAST(COALESCE(SUM(CASE WHEN status_class = 'success' THEN requests ELSE 0 END), 0) AS BIGINT) AS successful_requests,
           CAST(COALESCE(SUM(CASE WHEN status_class = 'failure' THEN requests ELSE 0 END), 0) AS BIGINT) AS failed_requests,
           CAST(COALESCE(SUM(input_tokens), 0) AS BIGINT) AS input_tokens,
           CAST(COALESCE(SUM(output_tokens), 0) AS BIGINT) AS output_tokens,
           CAST(COALESCE(SUM(cost_micros), 0) AS BIGINT) AS cost_micros
      FROM enriched
     GROUP BY GROUPING SETS ((), (model), (day_bucket), (error_bucket))
    HAVING GROUPING(error_bucket) = 1 OR error_bucket IS NOT NULL
),
ranked AS (
    SELECT grouped.*,
           ROW_NUMBER() OVER (
               PARTITION BY bucket_kind ORDER BY requests DESC, name ASC
           ) AS bucket_rank
      FROM grouped
)
SELECT bucket_kind, name, day_bucket, requests, successful_requests,
       failed_requests, input_tokens, output_tokens, cost_micros
  FROM ranked
 WHERE bucket_kind NOT IN ('model', 'error') OR bucket_rank <= 100
 ORDER BY CASE bucket_kind
              WHEN 'summary' THEN 0 WHEN 'model' THEN 1
              WHEN 'day' THEN 2 ELSE 3
          END,
          CASE WHEN bucket_kind = 'day' THEN day_bucket ELSE 0 END ASC,
          requests DESC,
          name ASC
"#
            ),
            DatabaseBackend::Sqlite => format!(
                r#"
WITH filtered_activity AS MATERIALIZED ({activity_source}),
grouped AS (
    SELECT 'summary' AS bucket_kind,
           '' AS name,
           CAST(-1 AS BIGINT) AS day_bucket,
           CAST(COALESCE(SUM(requests), 0) AS BIGINT) AS requests,
           CAST(COALESCE(SUM(CASE WHEN status_class = 'success' THEN requests ELSE 0 END), 0) AS BIGINT) AS successful_requests,
           CAST(COALESCE(SUM(CASE WHEN status_class = 'failure' THEN requests ELSE 0 END), 0) AS BIGINT) AS failed_requests,
           CAST(COALESCE(SUM(input_tokens), 0) AS BIGINT) AS input_tokens,
           CAST(COALESCE(SUM(output_tokens), 0) AS BIGINT) AS output_tokens,
           CAST(COALESCE(SUM(cost_micros), 0) AS BIGINT) AS cost_micros
      FROM filtered_activity
    UNION ALL
    SELECT 'model', model, -1, SUM(requests),
           SUM(CASE WHEN status_class = 'success' THEN requests ELSE 0 END),
           SUM(CASE WHEN status_class = 'failure' THEN requests ELSE 0 END),
           SUM(input_tokens), SUM(output_tokens), SUM(cost_micros)
      FROM filtered_activity GROUP BY model
    UNION ALL
    SELECT 'day', '', created_at / 86400000, SUM(requests),
           SUM(CASE WHEN status_class = 'success' THEN requests ELSE 0 END),
           SUM(CASE WHEN status_class = 'failure' THEN requests ELSE 0 END),
           SUM(input_tokens), SUM(output_tokens), SUM(cost_micros)
      FROM filtered_activity GROUP BY created_at / 86400000
    UNION ALL
    SELECT 'error', error_code, -1, SUM(requests),
           SUM(CASE WHEN status_class = 'success' THEN requests ELSE 0 END),
           SUM(CASE WHEN status_class = 'failure' THEN requests ELSE 0 END),
           SUM(input_tokens), SUM(output_tokens), SUM(cost_micros)
      FROM filtered_activity WHERE error_code <> '' GROUP BY error_code
),
ranked AS (
    SELECT grouped.*,
           ROW_NUMBER() OVER (
               PARTITION BY bucket_kind ORDER BY requests DESC, name ASC
           ) AS bucket_rank
      FROM grouped
)
SELECT bucket_kind, name, day_bucket, requests, successful_requests,
       failed_requests, input_tokens, output_tokens, cost_micros
  FROM ranked
 WHERE bucket_kind NOT IN ('model', 'error') OR bucket_rank <= 100
 ORDER BY CASE bucket_kind
              WHEN 'summary' THEN 0 WHEN 'model' THEN 1
              WHEN 'day' THEN 2 ELSE 3
          END,
          CASE WHEN bucket_kind = 'day' THEN day_bucket ELSE 0 END ASC,
          requests DESC,
          name ASC
"#
            ),
        };
        let rows = bind_activity_filter!(sqlx::query(&stats_sql))
            .fetch_all(&self.pool)
            .await?;
        let mut summary = None;
        let mut by_model = Vec::new();
        let mut by_day = Vec::new();
        let mut errors = Vec::new();
        for row in rows {
            let bucket_kind: String = row.try_get("bucket_kind")?;
            let requests: i64 = row.try_get("requests")?;
            let input_tokens: i64 = row.try_get("input_tokens")?;
            let output_tokens: i64 = row.try_get("output_tokens")?;
            let cost_micros: i64 = row.try_get("cost_micros")?;
            if bucket_kind == "summary" {
                summary = Some(StatsSummary {
                    total_requests: requests,
                    successful_requests: row.try_get("successful_requests")?,
                    failed_requests: row.try_get("failed_requests")?,
                    input_tokens,
                    output_tokens,
                    total_cost: micros_to_decimal_string(cost_micros),
                });
                continue;
            }
            let name = if bucket_kind == "day" {
                let day_bucket: i64 = row.try_get("day_bucket")?;
                chrono::DateTime::from_timestamp(day_bucket.saturating_mul(86_400), 0)
                    .map(|value| value.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| "unknown".to_owned())
            } else {
                row.try_get("name")?
            };
            let bucket = StatsBucket {
                name,
                requests,
                input_tokens,
                output_tokens,
                cost: micros_to_decimal_string(cost_micros),
            };
            match bucket_kind.as_str() {
                "model" => by_model.push(bucket),
                "day" => by_day.push(bucket),
                "error" => errors.push(bucket),
                _ => return Err(AppError::Internal),
            }
        }
        let summary = summary.ok_or(AppError::Internal)?;
        Ok(OperatorStats {
            summary,
            by_model,
            by_day,
            errors,
        })
    }

    pub async fn stats(&self, key_id: Uuid) -> Result<SelfStats, AppError> {
        let key_id = key_id.to_string();
        let summary_row = sqlx::query(
            "SELECT CAST(COALESCE(SUM(total_requests), 0) AS BIGINT) AS total_requests, CAST(COALESCE(SUM(successful_requests), 0) AS BIGINT) AS successful_requests, CAST(COALESCE(SUM(failed_requests), 0) AS BIGINT) AS failed_requests, CAST(COALESCE(SUM(input_tokens), 0) AS BIGINT) AS input_tokens, CAST(COALESCE(SUM(output_tokens), 0) AS BIGINT) AS output_tokens, CAST(COALESCE(SUM(cost_micros), 0) AS BIGINT) AS cost_micros FROM (SELECT COALESCE(SUM(requests), 0) AS total_requests, COALESCE(SUM(CASE WHEN status_class = 'success' THEN requests ELSE 0 END), 0) AS successful_requests, COALESCE(SUM(CASE WHEN status_class = 'failure' THEN requests ELSE 0 END), 0) AS failed_requests, COALESCE(SUM(input_tokens), 0) AS input_tokens, COALESCE(SUM(output_tokens), 0) AS output_tokens, COALESCE(SUM(cost_micros), 0) AS cost_micros FROM usage_daily_aggregates WHERE key_id = $1 UNION ALL SELECT COALESCE(SUM(requests), 0) AS total_requests, COALESCE(SUM(CASE WHEN status_class = 'success' THEN requests ELSE 0 END), 0) AS successful_requests, COALESCE(SUM(CASE WHEN status_class = 'failure' THEN requests ELSE 0 END), 0) AS failed_requests, 0 AS input_tokens, 0 AS output_tokens, COALESCE(SUM(cost_micros), 0) AS cost_micros FROM generation_daily_aggregates WHERE key_id = $2) AS totals",
        )
        .bind(&key_id)
        .bind(&key_id)
        .fetch_one(&self.pool)
        .await?;
        let summary = StatsSummary {
            total_requests: summary_row.try_get("total_requests")?,
            successful_requests: summary_row.try_get("successful_requests")?,
            failed_requests: summary_row.try_get("failed_requests")?,
            input_tokens: summary_row.try_get("input_tokens")?,
            output_tokens: summary_row.try_get("output_tokens")?,
            total_cost: micros_to_decimal_string(summary_row.try_get("cost_micros")?),
        };

        let model_rows = sqlx::query(
            "SELECT name, CAST(SUM(requests) AS BIGINT) AS requests, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost_micros) AS BIGINT) AS cost_micros FROM (SELECT model AS name, requests, input_tokens, output_tokens, cost_micros FROM usage_daily_aggregates WHERE key_id = $1 UNION ALL SELECT model AS name, requests, 0 AS input_tokens, 0 AS output_tokens, cost_micros FROM generation_daily_aggregates WHERE key_id = $2) AS model_totals GROUP BY name ORDER BY requests DESC, name ASC",
        )
        .bind(&key_id)
        .bind(&key_id)
        .fetch_all(&self.pool)
        .await?;
        let by_model = aggregate_buckets(model_rows)?;

        let day_rows = sqlx::query(
            "SELECT day_bucket, CAST(SUM(requests) AS BIGINT) AS requests, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost_micros) AS BIGINT) AS cost_micros FROM (SELECT day_bucket, requests, input_tokens, output_tokens, cost_micros FROM usage_daily_aggregates WHERE key_id = $1 UNION ALL SELECT day_bucket, requests, 0 AS input_tokens, 0 AS output_tokens, cost_micros FROM generation_daily_aggregates WHERE key_id = $2) AS day_totals GROUP BY day_bucket ORDER BY day_bucket ASC",
        )
        .bind(&key_id)
        .bind(&key_id)
        .fetch_all(&self.pool)
        .await?;
        let by_day = day_rows
            .into_iter()
            .map(|row| {
                let day_bucket: i64 = row.try_get("day_bucket")?;
                let name = chrono::DateTime::from_timestamp(day_bucket.saturating_mul(86_400), 0)
                    .map(|value| value.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| "unknown".to_owned());
                aggregate_bucket(row, name)
            })
            .collect::<Result<Vec<_>, AppError>>()?;

        let error_rows = sqlx::query(
            "SELECT name, CAST(SUM(requests) AS BIGINT) AS requests, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost_micros) AS BIGINT) AS cost_micros FROM (SELECT error_code AS name, requests, input_tokens, output_tokens, cost_micros FROM usage_daily_aggregates WHERE key_id = $1 AND error_code <> '' UNION ALL SELECT error_code AS name, requests, 0 AS input_tokens, 0 AS output_tokens, cost_micros FROM generation_daily_aggregates WHERE key_id = $2 AND status_class = 'failure' AND error_code <> '') AS error_totals GROUP BY name ORDER BY requests DESC, name ASC",
        )
        .bind(&key_id)
        .bind(&key_id)
        .fetch_all(&self.pool)
        .await?;
        let errors = aggregate_buckets(error_rows)?;

        Ok(SelfStats {
            key_id: parse_uuid(key_id)?,
            summary,
            by_model,
            by_day,
            errors,
        })
    }

    pub async fn operator_stats(
        &self,
        tenant_external_id: &str,
    ) -> Result<OperatorStats, AppError> {
        let summary_row = sqlx::query(
            "SELECT CAST(COALESCE(SUM(total_requests), 0) AS BIGINT) AS total_requests, CAST(COALESCE(SUM(successful_requests), 0) AS BIGINT) AS successful_requests, CAST(COALESCE(SUM(failed_requests), 0) AS BIGINT) AS failed_requests, CAST(COALESCE(SUM(input_tokens), 0) AS BIGINT) AS input_tokens, CAST(COALESCE(SUM(output_tokens), 0) AS BIGINT) AS output_tokens, CAST(COALESCE(SUM(cost_micros), 0) AS BIGINT) AS cost_micros FROM (SELECT COALESCE(SUM(a.requests), 0) AS total_requests, COALESCE(SUM(CASE WHEN a.status_class = 'success' THEN a.requests ELSE 0 END), 0) AS successful_requests, COALESCE(SUM(CASE WHEN a.status_class = 'failure' THEN a.requests ELSE 0 END), 0) AS failed_requests, COALESCE(SUM(a.input_tokens), 0) AS input_tokens, COALESCE(SUM(a.output_tokens), 0) AS output_tokens, COALESCE(SUM(a.cost_micros), 0) AS cost_micros FROM usage_daily_aggregates a JOIN key_records k ON k.id = a.key_id JOIN tenants t ON t.id = k.tenant_id WHERE t.external_id = $1 UNION ALL SELECT COALESCE(SUM(a.requests), 0) AS total_requests, COALESCE(SUM(CASE WHEN a.status_class = 'success' THEN a.requests ELSE 0 END), 0) AS successful_requests, COALESCE(SUM(CASE WHEN a.status_class = 'failure' THEN a.requests ELSE 0 END), 0) AS failed_requests, 0 AS input_tokens, 0 AS output_tokens, COALESCE(SUM(a.cost_micros), 0) AS cost_micros FROM generation_daily_aggregates a JOIN tenants t ON t.id = a.tenant_id WHERE t.external_id = $2) AS totals",
        )
        .bind(tenant_external_id)
        .bind(tenant_external_id)
        .fetch_one(&self.pool)
        .await?;
        let summary = StatsSummary {
            total_requests: summary_row.try_get("total_requests")?,
            successful_requests: summary_row.try_get("successful_requests")?,
            failed_requests: summary_row.try_get("failed_requests")?,
            input_tokens: summary_row.try_get("input_tokens")?,
            output_tokens: summary_row.try_get("output_tokens")?,
            total_cost: micros_to_decimal_string(summary_row.try_get("cost_micros")?),
        };
        let model_rows = sqlx::query(
            "SELECT name, CAST(SUM(requests) AS BIGINT) AS requests, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost_micros) AS BIGINT) AS cost_micros FROM (SELECT a.model AS name, a.requests, a.input_tokens, a.output_tokens, a.cost_micros FROM usage_daily_aggregates a JOIN key_records k ON k.id = a.key_id JOIN tenants t ON t.id = k.tenant_id WHERE t.external_id = $1 UNION ALL SELECT a.model AS name, a.requests, 0 AS input_tokens, 0 AS output_tokens, a.cost_micros FROM generation_daily_aggregates a JOIN tenants t ON t.id = a.tenant_id WHERE t.external_id = $2) AS model_totals GROUP BY name ORDER BY requests DESC, name ASC",
        )
        .bind(tenant_external_id)
        .bind(tenant_external_id)
        .fetch_all(&self.pool)
        .await?;
        let by_model = aggregate_buckets(model_rows)?;
        let day_rows = sqlx::query(
            "SELECT day_bucket, CAST(SUM(requests) AS BIGINT) AS requests, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost_micros) AS BIGINT) AS cost_micros FROM (SELECT a.day_bucket, a.requests, a.input_tokens, a.output_tokens, a.cost_micros FROM usage_daily_aggregates a JOIN key_records k ON k.id = a.key_id JOIN tenants t ON t.id = k.tenant_id WHERE t.external_id = $1 UNION ALL SELECT a.day_bucket, a.requests, 0 AS input_tokens, 0 AS output_tokens, a.cost_micros FROM generation_daily_aggregates a JOIN tenants t ON t.id = a.tenant_id WHERE t.external_id = $2) AS day_totals GROUP BY day_bucket ORDER BY day_bucket ASC",
        )
        .bind(tenant_external_id)
        .bind(tenant_external_id)
        .fetch_all(&self.pool)
        .await?;
        let by_day = day_rows
            .into_iter()
            .map(|row| {
                let day_bucket: i64 = row.try_get("day_bucket")?;
                let name = chrono::DateTime::from_timestamp(day_bucket.saturating_mul(86_400), 0)
                    .map(|value| value.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| "unknown".to_owned());
                aggregate_bucket(row, name)
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        let error_rows = sqlx::query(
            "SELECT name, CAST(SUM(requests) AS BIGINT) AS requests, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost_micros) AS BIGINT) AS cost_micros FROM (SELECT a.error_code AS name, a.requests, a.input_tokens, a.output_tokens, a.cost_micros FROM usage_daily_aggregates a JOIN key_records k ON k.id = a.key_id JOIN tenants t ON t.id = k.tenant_id WHERE t.external_id = $1 AND a.error_code <> '' UNION ALL SELECT a.error_code AS name, a.requests, 0 AS input_tokens, 0 AS output_tokens, a.cost_micros FROM generation_daily_aggregates a JOIN tenants t ON t.id = a.tenant_id WHERE t.external_id = $2 AND a.status_class = 'failure' AND a.error_code <> '') AS error_totals GROUP BY name ORDER BY requests DESC, name ASC",
        )
        .bind(tenant_external_id)
        .bind(tenant_external_id)
        .fetch_all(&self.pool)
        .await?;
        let errors = aggregate_buckets(error_rows)?;
        Ok(OperatorStats {
            summary,
            by_model,
            by_day,
            errors,
        })
    }

    pub async fn global_operator_stats(&self) -> Result<OperatorStats, AppError> {
        let summary_row = sqlx::query(
            "SELECT CAST(COALESCE(SUM(total_requests), 0) AS BIGINT) AS total_requests, CAST(COALESCE(SUM(successful_requests), 0) AS BIGINT) AS successful_requests, CAST(COALESCE(SUM(failed_requests), 0) AS BIGINT) AS failed_requests, CAST(COALESCE(SUM(input_tokens), 0) AS BIGINT) AS input_tokens, CAST(COALESCE(SUM(output_tokens), 0) AS BIGINT) AS output_tokens, CAST(COALESCE(SUM(cost_micros), 0) AS BIGINT) AS cost_micros FROM (SELECT COALESCE(SUM(requests), 0) AS total_requests, COALESCE(SUM(CASE WHEN status_class = 'success' THEN requests ELSE 0 END), 0) AS successful_requests, COALESCE(SUM(CASE WHEN status_class = 'failure' THEN requests ELSE 0 END), 0) AS failed_requests, COALESCE(SUM(input_tokens), 0) AS input_tokens, COALESCE(SUM(output_tokens), 0) AS output_tokens, COALESCE(SUM(cost_micros), 0) AS cost_micros FROM usage_daily_aggregates UNION ALL SELECT COALESCE(SUM(requests), 0) AS total_requests, COALESCE(SUM(CASE WHEN status_class = 'success' THEN requests ELSE 0 END), 0) AS successful_requests, COALESCE(SUM(CASE WHEN status_class = 'failure' THEN requests ELSE 0 END), 0) AS failed_requests, 0 AS input_tokens, 0 AS output_tokens, COALESCE(SUM(cost_micros), 0) AS cost_micros FROM generation_daily_aggregates) AS totals",
        )
        .fetch_one(&self.pool)
        .await?;
        let summary = StatsSummary {
            total_requests: summary_row.try_get("total_requests")?,
            successful_requests: summary_row.try_get("successful_requests")?,
            failed_requests: summary_row.try_get("failed_requests")?,
            input_tokens: summary_row.try_get("input_tokens")?,
            output_tokens: summary_row.try_get("output_tokens")?,
            total_cost: micros_to_decimal_string(summary_row.try_get("cost_micros")?),
        };
        let model_rows = sqlx::query(
            "SELECT name, CAST(SUM(requests) AS BIGINT) AS requests, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost_micros) AS BIGINT) AS cost_micros FROM (SELECT model AS name, requests, input_tokens, output_tokens, cost_micros FROM usage_daily_aggregates UNION ALL SELECT model AS name, requests, 0 AS input_tokens, 0 AS output_tokens, cost_micros FROM generation_daily_aggregates) AS model_totals GROUP BY name ORDER BY requests DESC, name ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        let by_model = aggregate_buckets(model_rows)?;
        let day_rows = sqlx::query(
            "SELECT day_bucket, CAST(SUM(requests) AS BIGINT) AS requests, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost_micros) AS BIGINT) AS cost_micros FROM (SELECT day_bucket, requests, input_tokens, output_tokens, cost_micros FROM usage_daily_aggregates UNION ALL SELECT day_bucket, requests, 0 AS input_tokens, 0 AS output_tokens, cost_micros FROM generation_daily_aggregates) AS day_totals GROUP BY day_bucket ORDER BY day_bucket ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        let by_day = day_rows
            .into_iter()
            .map(|row| {
                let day_bucket: i64 = row.try_get("day_bucket")?;
                let name = chrono::DateTime::from_timestamp(day_bucket.saturating_mul(86_400), 0)
                    .map(|value| value.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| "unknown".to_owned());
                aggregate_bucket(row, name)
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        let error_rows = sqlx::query(
            "SELECT name, CAST(SUM(requests) AS BIGINT) AS requests, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost_micros) AS BIGINT) AS cost_micros FROM (SELECT error_code AS name, requests, input_tokens, output_tokens, cost_micros FROM usage_daily_aggregates WHERE error_code <> '' UNION ALL SELECT error_code AS name, requests, 0 AS input_tokens, 0 AS output_tokens, cost_micros FROM generation_daily_aggregates WHERE status_class = 'failure' AND error_code <> '') AS error_totals GROUP BY name ORDER BY requests DESC, name ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(OperatorStats {
            summary,
            by_model,
            by_day,
            errors: aggregate_buckets(error_rows)?,
        })
    }

    /// Bounded, process-level and active-queue gauges for the Prometheus
    /// endpoint. No tenant, credential, model or request identifiers are read.
    pub async fn runtime_metrics(
        &self,
    ) -> Result<crate::metrics::DatabaseRuntimeMetrics, AppError> {
        let row = sqlx::query(
            "SELECT (SELECT COUNT(*) FROM generation_jobs WHERE status IN ('preparing', 'queued')) AS queued_jobs, (SELECT COUNT(*) FROM generation_jobs WHERE status IN ('submitting', 'running')) AS running_jobs",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(crate::metrics::DatabaseRuntimeMetrics {
            pool_size: self.pool.size(),
            pool_idle: self.pool.num_idle(),
            queued_jobs: row.try_get("queued_jobs")?,
            running_jobs: row.try_get("running_jobs")?,
        })
    }
}

fn aggregate_buckets(rows: Vec<sqlx::any::AnyRow>) -> Result<Vec<StatsBucket>, AppError> {
    rows.into_iter()
        .map(|row| {
            let name: String = row.try_get("name")?;
            aggregate_bucket(row, name)
        })
        .collect()
}

fn aggregate_bucket(row: sqlx::any::AnyRow, name: String) -> Result<StatsBucket, AppError> {
    Ok(StatsBucket {
        name,
        requests: row.try_get("requests")?,
        input_tokens: row.try_get("input_tokens")?,
        output_tokens: row.try_get("output_tokens")?,
        cost: micros_to_decimal_string(row.try_get("cost_micros")?),
    })
}

fn validate_stats_filter(filter: &StatsFilter) -> Result<(), AppError> {
    let from = filter
        .from_created_at
        .ok_or_else(|| AppError::BadRequest("from_created_at is required for statistics".into()))?;
    let to = filter
        .to_created_at
        .ok_or_else(|| AppError::BadRequest("to_created_at is required for statistics".into()))?;
    if from < 0 || to < 0 || from > to {
        return Err(AppError::BadRequest(
            "statistics require a valid non-negative from_created_at/to_created_at range".into(),
        ));
    }
    if to.saturating_sub(from) > MAX_STATS_RANGE_MILLIS {
        return Err(AppError::BadRequest(
            "statistics range must not exceed 93 days".into(),
        ));
    }
    if filter
        .status
        .as_deref()
        .is_some_and(|value| !matches!(value, "success" | "error" | "pending"))
    {
        return Err(AppError::BadRequest(
            "status must be success, error, or pending".into(),
        ));
    }
    validate_numeric_range(
        "duration_ms",
        filter.min_duration_ms,
        filter.max_duration_ms,
    )?;
    validate_numeric_range("cost", filter.min_cost_micros, filter.max_cost_micros)?;
    for (name, value) in [
        ("model", filter.model.as_deref()),
        ("protocol", filter.protocol.as_deref()),
        ("error_code", filter.error_code.as_deref()),
        ("key_alias", filter.key_alias.as_deref()),
        ("principal", filter.principal.as_deref()),
    ] {
        if value.is_some_and(|value| {
            value.trim().is_empty() || value.len() > 200 || value.chars().any(char::is_control)
        }) {
            return Err(AppError::BadRequest(format!(
                "{name} must contain 1 to 200 non-control characters"
            )));
        }
    }
    Ok(())
}

pub(crate) fn validate_numeric_range(
    name: &str,
    minimum: Option<i64>,
    maximum: Option<i64>,
) -> Result<(), AppError> {
    if minimum.is_some_and(|value| value < 0) || maximum.is_some_and(|value| value < 0) {
        return Err(AppError::BadRequest(format!(
            "{name} bounds must not be negative"
        )));
    }
    if minimum
        .zip(maximum)
        .is_some_and(|(minimum, maximum)| minimum > maximum)
    {
        return Err(AppError::BadRequest(format!(
            "minimum {name} must not exceed maximum {name}"
        )));
    }
    Ok(())
}
