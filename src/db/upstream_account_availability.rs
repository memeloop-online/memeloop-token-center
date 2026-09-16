use std::collections::BTreeMap;

use serde::Serialize;
use sqlx::{Row, any::AnyRow};

use super::effective_cost::{
    request_adjustment_join_sql, request_fact_effective_cost_sql,
    request_rollup_effective_cost_for_bucket_sql,
};
use super::{
    AppError, Database, DatabaseBackend, MAX_STATS_RANGE_MILLIS, micros_to_decimal_string,
    unix_millis,
};
use crate::model::{MonitoringMetrics, MonitoringTerminalOutcome, TokenUsage, UsageAnalysisCost};

const HOUR_MILLIS: i64 = 3_600_000;
const DAY_MILLIS: i64 = 86_400_000;
const HOUR_RANGE_LIMIT: i64 = 31 * DAY_MILLIS;

/// An exact inclusive traffic window for the tenant's current upstream
/// accounts. This is deliberately account-scoped rather than a top-model
/// projection: every current account receives one result, including accounts
/// with no terminal facts in the window.
#[derive(Clone, Debug)]
pub struct UpstreamAccountAvailabilityFilter {
    pub from_created_at: i64,
    pub to_created_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct UpstreamAccountAvailabilityWindow {
    pub contract_version: String,
    pub generated_at: i64,
    pub tenant_external_id: String,
    pub from_created_at: i64,
    pub to_created_at: i64,
    pub granularity: String,
    pub latency_is_approximate: bool,
    pub latency_method: String,
    pub accounts: Vec<UpstreamAccountAvailability>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UpstreamAccountAvailability {
    pub upstream_account_id: String,
    pub metrics: MonitoringMetrics,
    /// The five newest terminal facts across every model on this account.
    /// `source` remains explicit so a generation fact is never presented as a
    /// synchronous request.
    pub terminal_outcomes: Vec<MonitoringTerminalOutcome>,
}

#[derive(Clone, Copy, Debug)]
enum AvailabilityGranularity {
    Hour,
    Day,
}

impl AvailabilityGranularity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hour => "hour",
            Self::Day => "day",
        }
    }

    fn bucket_millis(self) -> i64 {
        match self {
            Self::Hour => HOUR_MILLIS,
            Self::Day => DAY_MILLIS,
        }
    }
}

struct AvailabilityRange {
    from_created_at: i64,
    to_created_at: i64,
    granularity: AvailabilityGranularity,
}

#[derive(Clone, Copy)]
struct BucketPlan {
    rollup_from_bucket: i64,
    rollup_to_bucket: i64,
    left_from_created_at: i64,
    left_to_created_at: i64,
    right_from_created_at: i64,
    right_to_created_at: i64,
}

impl BucketPlan {
    /// Match the monitoring snapshot's exact inclusive window semantics:
    /// complete buckets use immutable rollups and only the two partial edges
    /// read terminal facts.
    fn new(from_created_at: i64, to_created_at: i64, bucket_millis: i64) -> Self {
        let bucket_millis = i128::from(bucket_millis);
        let from = i128::from(from_created_at);
        let to_exclusive = to_created_at
            .checked_add(1)
            .map(i128::from)
            .unwrap_or_else(|| i128::from(i64::MAX) + 1);
        let from_bucket = from.div_euclid(bucket_millis);
        let rollup_from = if from.rem_euclid(bucket_millis) == 0 {
            from_bucket
        } else {
            from_bucket.saturating_add(1)
        };
        let rollup_to = to_exclusive.div_euclid(bucket_millis);
        let full_from = rollup_from.saturating_mul(bucket_millis);
        let full_to = rollup_to.saturating_mul(bucket_millis);
        let rollup_from_bucket = i64::try_from(rollup_from)
            .expect("non-negative timestamps have representable bucket indices");
        let rollup_to_bucket = i64::try_from(rollup_to)
            .expect("non-negative timestamps have representable bucket indices");
        if full_from < full_to {
            let (left_from_created_at, left_to_created_at) =
                inclusive_bounds(from, full_from.min(to_exclusive));
            let (right_from_created_at, right_to_created_at) =
                inclusive_bounds(full_to.max(from), to_exclusive);
            Self {
                rollup_from_bucket,
                rollup_to_bucket,
                left_from_created_at,
                left_to_created_at,
                right_from_created_at,
                right_to_created_at,
            }
        } else {
            Self {
                rollup_from_bucket,
                rollup_to_bucket,
                left_from_created_at: from_created_at,
                left_to_created_at: to_created_at,
                right_from_created_at: 0,
                right_to_created_at: -1,
            }
        }
    }
}

fn inclusive_bounds(from: i128, to_exclusive: i128) -> (i64, i64) {
    if from >= to_exclusive {
        return (0, -1);
    }
    (
        i64::try_from(from).expect("fact-bound start is representable"),
        i64::try_from(to_exclusive - 1).expect("fact-bound end is representable"),
    )
}

#[derive(Default)]
struct MetricsAccumulator {
    requests: i64,
    successful_requests: i64,
    failed_requests: i64,
    input_tokens: i64,
    output_tokens: i64,
    cached_input_tokens: i64,
    cache_write_tokens: i64,
    duration_count: i64,
    duration_sum_ms: i64,
    duration_buckets: [i64; 12],
    costs: BTreeMap<String, i64>,
}

impl MetricsAccumulator {
    fn accumulate(&mut self, row: &AnyRow) -> Result<(), AppError> {
        let currency: Option<String> = row.try_get("currency")?;
        let Some(currency) = currency else {
            // The left join deliberately emits one null metric row for a
            // current account with no terminal traffic in the selected window.
            return Ok(());
        };
        if currency.is_empty() {
            return Err(AppError::Internal);
        }
        self.requests = self.requests.saturating_add(row.try_get("requests")?);
        self.successful_requests = self
            .successful_requests
            .saturating_add(row.try_get("successful_requests")?);
        self.failed_requests = self
            .failed_requests
            .saturating_add(row.try_get("failed_requests")?);
        self.input_tokens = self
            .input_tokens
            .saturating_add(row.try_get("input_tokens")?);
        self.output_tokens = self
            .output_tokens
            .saturating_add(row.try_get("output_tokens")?);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(row.try_get("cached_input_tokens")?);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(row.try_get("cache_write_tokens")?);
        self.duration_count = self
            .duration_count
            .saturating_add(row.try_get("duration_count")?);
        self.duration_sum_ms = self
            .duration_sum_ms
            .saturating_add(row.try_get("duration_sum_ms")?);
        for (index, bucket) in self.duration_buckets.iter_mut().enumerate() {
            let column = format!("duration_bucket_{index}");
            *bucket = bucket.saturating_add(row.try_get(column.as_str())?);
        }
        let cost = self.costs.entry(currency).or_default();
        *cost = cost.saturating_add(row.try_get("cost_micros")?);
        Ok(())
    }

    fn finish(self) -> MonitoringMetrics {
        let usage = TokenUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_input_tokens: self.cached_input_tokens,
            cache_write_tokens: self.cache_write_tokens,
            service_tier: None,
        };
        MonitoringMetrics {
            requests: self.requests,
            successful_requests: self.successful_requests,
            failed_requests: self.failed_requests,
            total_tokens: usage.total_tokens(),
            cache_rate: usage.cache_rate(),
            avg_duration_ms: (self.duration_count > 0)
                .then(|| self.duration_sum_ms as f64 / self.duration_count as f64),
            p95_duration_ms: approximate_quantile(95, self.duration_count, &self.duration_buckets),
            p95_is_capped: super::usage_analysis::histogram_p95_is_capped(
                self.duration_count,
                &self.duration_buckets,
            ),
            costs: self
                .costs
                .into_iter()
                .map(|(currency, micros)| UsageAnalysisCost {
                    currency,
                    cost: micros_to_decimal_string(micros),
                })
                .collect(),
        }
    }
}

fn approximate_quantile(percentile: i64, count: i64, buckets: &[i64; 12]) -> Option<i64> {
    if count <= 0 {
        return None;
    }
    let target = count
        .saturating_mul(percentile)
        .saturating_add(99)
        .div_euclid(100);
    let upper_bounds = [
        10, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000, 60_000,
    ];
    let mut cumulative = 0_i64;
    for (bucket, upper_bound) in buckets.iter().zip(upper_bounds) {
        cumulative = cumulative.saturating_add(*bucket);
        if cumulative >= target {
            return Some(upper_bound);
        }
    }
    Some(60_000)
}

impl Database {
    /// Read the complete current account set for one authorized tenant. This
    /// issues two bounded statements regardless of account count: one exact
    /// window aggregate and one per-account top-five terminal fact query.
    pub async fn upstream_account_availability(
        &self,
        tenant_external_id: &str,
        filter: UpstreamAccountAvailabilityFilter,
    ) -> Result<UpstreamAccountAvailabilityWindow, AppError> {
        let range = validate_range(filter)?;
        let plan = BucketPlan::new(
            range.from_created_at,
            range.to_created_at,
            range.granularity.bucket_millis(),
        );
        let mut transaction = self.pool.begin().await?;
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *transaction)
                .await?;
        }

        let rows = sqlx::query(sqlx::AssertSqlSafe(window_metrics_sql(range.granularity)))
            .bind(tenant_external_id)
            .bind(plan.rollup_from_bucket)
            .bind(plan.rollup_to_bucket)
            .bind(plan.left_from_created_at)
            .bind(plan.left_to_created_at)
            .bind(plan.right_from_created_at)
            .bind(plan.right_to_created_at)
            .fetch_all(&mut *transaction)
            .await?;
        let mut metrics_by_account = BTreeMap::<String, MetricsAccumulator>::new();
        for row in rows {
            let account_id: String = row.try_get("upstream_account_id")?;
            metrics_by_account
                .entry(account_id)
                .or_default()
                .accumulate(&row)?;
        }

        let outcome_rows = sqlx::query(sqlx::AssertSqlSafe(terminal_outcomes_sql()))
            .bind(tenant_external_id)
            .bind(range.from_created_at)
            .bind(range.to_created_at)
            .fetch_all(&mut *transaction)
            .await?;
        let mut outcomes_by_account = BTreeMap::<String, Vec<MonitoringTerminalOutcome>>::new();
        for row in outcome_rows {
            let account_id: String = row.try_get("upstream_account_id")?;
            let source: String = row.try_get("source")?;
            let status: String = row.try_get("status_class")?;
            if !matches!(source.as_str(), "request" | "generation")
                || !matches!(status.as_str(), "success" | "failure")
            {
                return Err(AppError::Internal);
            }
            let error_code: String = row.try_get("error_code")?;
            outcomes_by_account
                .entry(account_id)
                .or_default()
                .push(MonitoringTerminalOutcome {
                    id: row.try_get("id")?,
                    source,
                    created_at: row.try_get("created_at")?,
                    status,
                    duration_ms: row.try_get("duration_ms")?,
                    error_code: (!error_code.is_empty()).then_some(error_code),
                });
        }
        transaction.commit().await?;

        let accounts = metrics_by_account
            .into_iter()
            .map(
                |(upstream_account_id, metrics)| UpstreamAccountAvailability {
                    terminal_outcomes: outcomes_by_account
                        .remove(&upstream_account_id)
                        .unwrap_or_default(),
                    upstream_account_id,
                    metrics: metrics.finish(),
                },
            )
            .collect();

        Ok(UpstreamAccountAvailabilityWindow {
            contract_version: "upstream_account_availability_v1".to_owned(),
            generated_at: unix_millis(),
            tenant_external_id: tenant_external_id.to_owned(),
            from_created_at: range.from_created_at,
            to_created_at: range.to_created_at,
            granularity: range.granularity.as_str().to_owned(),
            latency_is_approximate: true,
            latency_method: "fixed_histogram_upper_bound_capped_60000ms".to_owned(),
            accounts,
        })
    }
}

fn validate_range(
    filter: UpstreamAccountAvailabilityFilter,
) -> Result<AvailabilityRange, AppError> {
    if filter.from_created_at < 0
        || filter.to_created_at < 0
        || filter.from_created_at > filter.to_created_at
    {
        return Err(AppError::BadRequest(
            "upstream availability requires a valid non-negative inclusive time window".into(),
        ));
    }
    let width = filter.to_created_at.saturating_sub(filter.from_created_at);
    if width > MAX_STATS_RANGE_MILLIS {
        return Err(AppError::BadRequest(
            "upstream availability window must not exceed 93 days".into(),
        ));
    }
    Ok(AvailabilityRange {
        from_created_at: filter.from_created_at,
        to_created_at: filter.to_created_at,
        granularity: if width <= HOUR_RANGE_LIMIT {
            AvailabilityGranularity::Hour
        } else {
            AvailabilityGranularity::Day
        },
    })
}

/// One account-complete aggregate. The account CTE is the tenant boundary and
/// makes the left join preserve unobserved current accounts without inventing
/// a terminal result. The four source arms exactly cover the inclusive window.
fn window_metrics_sql(granularity: AvailabilityGranularity) -> String {
    let (table, bucket_column, bucket_millis) = match granularity {
        AvailabilityGranularity::Hour => ("usage_analysis_hourly", "hour_bucket", HOUR_MILLIS),
        AvailabilityGranularity::Day => ("usage_analysis_daily", "day_bucket", DAY_MILLIS),
    };
    let effective_rollup_cost = request_rollup_effective_cost_for_bucket_sql(
        "aggregate",
        "rollup_fact",
        "rollup_request",
        "rollup_feed",
        "rollup_adjustments",
        bucket_column,
        bucket_millis,
        true,
    );
    let rollup = format!(
        r#"SELECT aggregate.upstream_account_id, aggregate.status_class, aggregate.currency,
                  aggregate.requests, aggregate.input_tokens, aggregate.output_tokens,
                  aggregate.cached_input_tokens, aggregate.cache_write_tokens,
                  aggregate.duration_count, aggregate.duration_sum_ms,
                  aggregate.duration_bucket_0, aggregate.duration_bucket_1, aggregate.duration_bucket_2,
                  aggregate.duration_bucket_3, aggregate.duration_bucket_4, aggregate.duration_bucket_5,
                  aggregate.duration_bucket_6, aggregate.duration_bucket_7, aggregate.duration_bucket_8,
                  aggregate.duration_bucket_9, aggregate.duration_bucket_10, aggregate.duration_bucket_11,
                  {effective_rollup_cost} AS cost_micros
            FROM {table} aggregate
             JOIN accounts account
               ON account.upstream_account_id = aggregate.upstream_account_id
              AND account.tenant_id = aggregate.tenant_id
            WHERE aggregate.tenant_id = (SELECT tenant_id FROM tenant_scope)
              AND aggregate.{bucket_column} >= $2
              AND aggregate.{bucket_column} < $3"#,
        effective_rollup_cost = effective_rollup_cost,
    );
    let left_requests = fact_metrics_sql("request_stats_facts", "f", "$4", "$5");
    let left_generations = fact_metrics_sql("generation_stats_facts", "f", "$4", "$5");
    let right_requests = fact_metrics_sql("request_stats_facts", "f", "$6", "$7");
    let right_generations = fact_metrics_sql("generation_stats_facts", "f", "$6", "$7");
    format!(
        r#"WITH tenant_scope AS MATERIALIZED (
    SELECT tenant.id AS tenant_id
      FROM tenants tenant
     WHERE tenant.external_id = $1
), accounts AS MATERIALIZED (
    SELECT account.id AS upstream_account_id, account.tenant_id
      FROM upstream_accounts account
      JOIN tenant_scope tenant ON tenant.tenant_id = account.tenant_id
), source AS MATERIALIZED (
    {rollup}
    UNION ALL
    {left_requests}
    UNION ALL
    {left_generations}
    UNION ALL
    {right_requests}
    UNION ALL
    {right_generations}
), metrics AS (
    SELECT upstream_account_id, currency,
           {metric_sums}
      FROM source
     GROUP BY upstream_account_id, currency
)
SELECT account.upstream_account_id, metrics.currency,
       COALESCE(metrics.requests, 0) AS requests,
       COALESCE(metrics.successful_requests, 0) AS successful_requests,
       COALESCE(metrics.failed_requests, 0) AS failed_requests,
       COALESCE(metrics.input_tokens, 0) AS input_tokens,
       COALESCE(metrics.output_tokens, 0) AS output_tokens,
       COALESCE(metrics.cached_input_tokens, 0) AS cached_input_tokens,
       COALESCE(metrics.cache_write_tokens, 0) AS cache_write_tokens,
       COALESCE(metrics.duration_count, 0) AS duration_count,
       COALESCE(metrics.duration_sum_ms, 0) AS duration_sum_ms,
       COALESCE(metrics.duration_bucket_0, 0) AS duration_bucket_0,
       COALESCE(metrics.duration_bucket_1, 0) AS duration_bucket_1,
       COALESCE(metrics.duration_bucket_2, 0) AS duration_bucket_2,
       COALESCE(metrics.duration_bucket_3, 0) AS duration_bucket_3,
       COALESCE(metrics.duration_bucket_4, 0) AS duration_bucket_4,
       COALESCE(metrics.duration_bucket_5, 0) AS duration_bucket_5,
       COALESCE(metrics.duration_bucket_6, 0) AS duration_bucket_6,
       COALESCE(metrics.duration_bucket_7, 0) AS duration_bucket_7,
       COALESCE(metrics.duration_bucket_8, 0) AS duration_bucket_8,
       COALESCE(metrics.duration_bucket_9, 0) AS duration_bucket_9,
       COALESCE(metrics.duration_bucket_10, 0) AS duration_bucket_10,
       COALESCE(metrics.duration_bucket_11, 0) AS duration_bucket_11,
       COALESCE(metrics.cost_micros, 0) AS cost_micros
  FROM accounts account
  LEFT JOIN metrics
    ON metrics.upstream_account_id = account.upstream_account_id
 ORDER BY account.upstream_account_id ASC, metrics.currency ASC"#,
        metric_sums = metric_sums(),
    )
}

fn fact_metrics_sql(table: &str, alias: &str, from_parameter: &str, to_parameter: &str) -> String {
    let is_request = table == "request_stats_facts";
    let token_projection = if table == "request_stats_facts" {
        format!(
            r#"CASE
                      WHEN {alias}.input_tokens >= {alias}.cached_input_tokens + {alias}.cache_write_tokens
                          THEN {alias}.input_tokens - {alias}.cached_input_tokens - {alias}.cache_write_tokens
                      ELSE 0
                  END AS input_tokens,
                  {alias}.output_tokens, {alias}.cached_input_tokens, {alias}.cache_write_tokens"#,
        )
    } else {
        r#"CAST(0 AS BIGINT) AS input_tokens,
                  CAST(0 AS BIGINT) AS output_tokens,
                  CAST(0 AS BIGINT) AS cached_input_tokens,
                  CAST(0 AS BIGINT) AS cache_write_tokens"#
            .to_owned()
    };
    let effective_cost = if is_request {
        request_fact_effective_cost_sql(alias, "billing_request", "billing_adjustments")
    } else {
        format!("{alias}.cost_micros")
    };
    let request_join = if is_request {
        let adjustment_joins =
            request_adjustment_join_sql(alias, "billing_feed", "billing_adjustments");
        format!(
            "LEFT JOIN request_records billing_request ON billing_request.id = {alias}.request_id AND billing_request.created_at = {alias}.created_at\n             {adjustment_joins}"
        )
    } else {
        String::new()
    };
    format!(
        r#"SELECT {alias}.upstream_account_id, {alias}.status_class, {alias}.currency,
                  CAST(1 AS BIGINT) AS requests,
                  {token_projection},
                  CAST(1 AS BIGINT) AS duration_count, {alias}.duration_ms AS duration_sum_ms,
                  CASE WHEN {alias}.duration_ms <= 10 THEN 1 ELSE 0 END AS duration_bucket_0,
                  CASE WHEN {alias}.duration_ms > 10 AND {alias}.duration_ms <= 50 THEN 1 ELSE 0 END AS duration_bucket_1,
                  CASE WHEN {alias}.duration_ms > 50 AND {alias}.duration_ms <= 100 THEN 1 ELSE 0 END AS duration_bucket_2,
                  CASE WHEN {alias}.duration_ms > 100 AND {alias}.duration_ms <= 250 THEN 1 ELSE 0 END AS duration_bucket_3,
                  CASE WHEN {alias}.duration_ms > 250 AND {alias}.duration_ms <= 500 THEN 1 ELSE 0 END AS duration_bucket_4,
                  CASE WHEN {alias}.duration_ms > 500 AND {alias}.duration_ms <= 1000 THEN 1 ELSE 0 END AS duration_bucket_5,
                  CASE WHEN {alias}.duration_ms > 1000 AND {alias}.duration_ms <= 2500 THEN 1 ELSE 0 END AS duration_bucket_6,
                  CASE WHEN {alias}.duration_ms > 2500 AND {alias}.duration_ms <= 5000 THEN 1 ELSE 0 END AS duration_bucket_7,
                  CASE WHEN {alias}.duration_ms > 5000 AND {alias}.duration_ms <= 10000 THEN 1 ELSE 0 END AS duration_bucket_8,
                  CASE WHEN {alias}.duration_ms > 10000 AND {alias}.duration_ms <= 30000 THEN 1 ELSE 0 END AS duration_bucket_9,
                  CASE WHEN {alias}.duration_ms > 30000 AND {alias}.duration_ms <= 60000 THEN 1 ELSE 0 END AS duration_bucket_10,
                  CASE WHEN {alias}.duration_ms > 60000 THEN 1 ELSE 0 END AS duration_bucket_11,
                  {effective_cost} AS cost_micros
             FROM {table} {alias}
             JOIN accounts account
               ON account.upstream_account_id = {alias}.upstream_account_id
              AND account.tenant_id = {alias}.tenant_id
             {request_join}
            WHERE CAST({from_parameter} AS BIGINT) <= CAST({to_parameter} AS BIGINT)
              AND {alias}.tenant_id = (SELECT tenant_id FROM tenant_scope)
              AND {alias}.created_at >= {from_parameter}
              AND {alias}.created_at <= {to_parameter}"#,
        effective_cost = effective_cost,
        request_join = request_join,
    )
}

/// The second and final query is deliberately account-partitioned rather than
/// model-partitioned. It reads only terminal fact projections and emits at
/// most five rows for every current account in the tenant.
fn terminal_outcomes_sql() -> &'static str {
    r#"WITH tenant_scope AS MATERIALIZED (
    SELECT tenant.id AS tenant_id
      FROM tenants tenant
     WHERE tenant.external_id = $1
), accounts AS MATERIALIZED (
    SELECT account.id AS upstream_account_id, account.tenant_id
      FROM upstream_accounts account
      JOIN tenant_scope tenant ON tenant.tenant_id = account.tenant_id
), terminal AS (
    SELECT fact.upstream_account_id, fact.request_id AS id, 'request' AS source,
           fact.created_at, fact.status_class, fact.duration_ms, fact.error_code
      FROM request_stats_facts fact
      JOIN accounts account
        ON account.upstream_account_id = fact.upstream_account_id
       AND account.tenant_id = fact.tenant_id
     WHERE fact.created_at >= $2
       AND fact.tenant_id = (SELECT tenant_id FROM tenant_scope)
       AND fact.created_at <= $3
    UNION ALL
    SELECT fact.upstream_account_id, fact.job_id AS id, 'generation' AS source,
           fact.created_at, fact.status_class, fact.duration_ms, fact.error_code
      FROM generation_stats_facts fact
      JOIN accounts account
        ON account.upstream_account_id = fact.upstream_account_id
       AND account.tenant_id = fact.tenant_id
     WHERE fact.created_at >= $2
       AND fact.tenant_id = (SELECT tenant_id FROM tenant_scope)
       AND fact.created_at <= $3
), ranked AS (
    SELECT upstream_account_id, id, source, created_at, status_class, duration_ms, error_code,
           ROW_NUMBER() OVER (
               PARTITION BY upstream_account_id
               ORDER BY created_at DESC, source ASC, id DESC
           ) AS terminal_rank
      FROM terminal
)
SELECT upstream_account_id, id, source, created_at, status_class, duration_ms, error_code
  FROM ranked
 WHERE terminal_rank <= 5
 ORDER BY upstream_account_id ASC, created_at DESC, source ASC, id DESC"#
}

fn metric_sums() -> &'static str {
    r#"CAST(COALESCE(SUM(requests), 0) AS BIGINT) AS requests,
       CAST(COALESCE(SUM(CASE WHEN status_class = 'success' THEN requests ELSE 0 END), 0) AS BIGINT) AS successful_requests,
       CAST(COALESCE(SUM(CASE WHEN status_class = 'failure' THEN requests ELSE 0 END), 0) AS BIGINT) AS failed_requests,
       CAST(COALESCE(SUM(input_tokens), 0) AS BIGINT) AS input_tokens,
       CAST(COALESCE(SUM(output_tokens), 0) AS BIGINT) AS output_tokens,
       CAST(COALESCE(SUM(cached_input_tokens), 0) AS BIGINT) AS cached_input_tokens,
       CAST(COALESCE(SUM(cache_write_tokens), 0) AS BIGINT) AS cache_write_tokens,
       CAST(COALESCE(SUM(duration_count), 0) AS BIGINT) AS duration_count,
       CAST(COALESCE(SUM(duration_sum_ms), 0) AS BIGINT) AS duration_sum_ms,
       CAST(COALESCE(SUM(duration_bucket_0), 0) AS BIGINT) AS duration_bucket_0,
       CAST(COALESCE(SUM(duration_bucket_1), 0) AS BIGINT) AS duration_bucket_1,
       CAST(COALESCE(SUM(duration_bucket_2), 0) AS BIGINT) AS duration_bucket_2,
       CAST(COALESCE(SUM(duration_bucket_3), 0) AS BIGINT) AS duration_bucket_3,
       CAST(COALESCE(SUM(duration_bucket_4), 0) AS BIGINT) AS duration_bucket_4,
       CAST(COALESCE(SUM(duration_bucket_5), 0) AS BIGINT) AS duration_bucket_5,
       CAST(COALESCE(SUM(duration_bucket_6), 0) AS BIGINT) AS duration_bucket_6,
       CAST(COALESCE(SUM(duration_bucket_7), 0) AS BIGINT) AS duration_bucket_7,
       CAST(COALESCE(SUM(duration_bucket_8), 0) AS BIGINT) AS duration_bucket_8,
       CAST(COALESCE(SUM(duration_bucket_9), 0) AS BIGINT) AS duration_bucket_9,
       CAST(COALESCE(SUM(duration_bucket_10), 0) AS BIGINT) AS duration_bucket_10,
       CAST(COALESCE(SUM(duration_bucket_11), 0) AS BIGINT) AS duration_bucket_11,
       CAST(COALESCE(SUM(cost_micros), 0) AS BIGINT) AS cost_micros"#
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn exact_window_uses_monitoring_bucket_boundaries() {
        let hourly = validate_range(UpstreamAccountAvailabilityFilter {
            from_created_at: 0,
            to_created_at: HOUR_RANGE_LIMIT,
        })
        .expect("hourly maximum is valid");
        assert!(matches!(hourly.granularity, AvailabilityGranularity::Hour));
        let daily = validate_range(UpstreamAccountAvailabilityFilter {
            from_created_at: 0,
            to_created_at: HOUR_RANGE_LIMIT + 1,
        })
        .expect("daily window is valid");
        assert!(matches!(daily.granularity, AvailabilityGranularity::Day));
        assert!(
            validate_range(UpstreamAccountAvailabilityFilter {
                from_created_at: 0,
                to_created_at: MAX_STATS_RANGE_MILLIS + 1,
            })
            .is_err()
        );
    }

    #[test]
    fn bucket_plan_covers_inclusive_edges_without_overlap_or_overflow() {
        for bucket in [HOUR_MILLIS, DAY_MILLIS] {
            for (from, to) in [
                (0, 0),
                (0, bucket - 1),
                (1, bucket - 2),
                (1, 3 * bucket + 1),
                (bucket, 3 * bucket - 1),
                (i64::MAX - bucket, i64::MAX),
            ] {
                let plan = BucketPlan::new(from, to, bucket);
                let mut intervals = Vec::new();
                if plan.left_from_created_at <= plan.left_to_created_at {
                    intervals.push((
                        i128::from(plan.left_from_created_at),
                        i128::from(plan.left_to_created_at) + 1,
                    ));
                }
                if plan.rollup_from_bucket < plan.rollup_to_bucket {
                    intervals.push((
                        i128::from(plan.rollup_from_bucket) * i128::from(bucket),
                        i128::from(plan.rollup_to_bucket) * i128::from(bucket),
                    ));
                }
                if plan.right_from_created_at <= plan.right_to_created_at {
                    intervals.push((
                        i128::from(plan.right_from_created_at),
                        i128::from(plan.right_to_created_at) + 1,
                    ));
                }
                let mut cursor = i128::from(from);
                for (start, end) in intervals {
                    assert_eq!(start, cursor, "window {from}..={to}, bucket {bucket}");
                    assert!(end > start);
                    cursor = end;
                }
                assert_eq!(cursor, i128::from(to) + 1);
            }
        }
    }

    #[test]
    fn account_window_sql_is_tenant_bound_complete_and_not_top_model_sampled() {
        for granularity in [AvailabilityGranularity::Hour, AvailabilityGranularity::Day] {
            let sql = window_metrics_sql(granularity);
            assert!(sql.contains("FROM tenants tenant"), "{sql}");
            assert!(sql.contains("tenant.external_id = $1"), "{sql}");
            assert!(sql.contains("tenant_id FROM tenant_scope"), "{sql}");
            assert!(sql.contains("LEFT JOIN metrics"), "{sql}");
            assert!(sql.contains("usage_analysis_"), "{sql}");
            assert!(sql.contains("request_stats_facts"), "{sql}");
            assert!(sql.contains("generation_stats_facts"), "{sql}");
            assert!(sql.contains("aggregate.cached_input_tokens"), "{sql}");
            assert!(sql.contains("SUM(cache_write_tokens)"), "{sql}");
            assert!(
                sql.contains("f.input_tokens - f.cached_input_tokens - f.cache_write_tokens"),
                "{sql}"
            );
            assert!(
                sql.contains("CAST(0 AS BIGINT) AS cached_input_tokens"),
                "{sql}"
            );
            assert!(!sql.contains("LIMIT 10"), "{sql}");
            assert!(!sql.contains("request_records"), "{sql}");
            assert!(!sql.contains("generation_jobs"), "{sql}");
        }
    }

    #[test]
    fn terminal_sql_combines_sources_and_limits_each_account_without_n_plus_one() {
        let sql = terminal_outcomes_sql();
        assert!(sql.contains("request_stats_facts"), "{sql}");
        assert!(sql.contains("generation_stats_facts"), "{sql}");
        assert!(sql.contains("PARTITION BY upstream_account_id"), "{sql}");
        assert!(sql.contains("terminal_rank <= 5"), "{sql}");
        assert!(sql.contains("tenant.external_id = $1"), "{sql}");
        assert!(sql.contains("tenant_id FROM tenant_scope"), "{sql}");
        assert!(!sql.contains("LIMIT 10"), "{sql}");
        assert!(!sql.contains("request_records"), "{sql}");
        assert!(!sql.contains("generation_jobs"), "{sql}");
    }

    #[tokio::test]
    async fn tenant_window_returns_every_current_account_and_only_its_five_newest_terminal_facts() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory
                .path()
                .join("upstream-account-availability.db")
                .display()
        );
        let database = Database::connect(&database_url)
            .await
            .expect("database connection");
        database.migrate().await.expect("database migration");
        assert_tenant_window(&database).await;
        assert_token_metrics_across_rollup_and_edges(&database).await;
    }

    #[tokio::test]
    async fn postgres_tenant_window_decodes_account_metrics_and_terminal_sources() {
        let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        let database = Database::connect(&database_url)
            .await
            .expect("PostgreSQL connection");
        database.migrate().await.expect("PostgreSQL migration");
        assert_tenant_window(&database).await;
        assert_token_metrics_across_rollup_and_edges(&database).await;
    }

    async fn assert_token_metrics_across_rollup_and_edges(database: &Database) {
        let tenant_id = Uuid::now_v7().to_string();
        let tenant_external_id = format!("availability-tokens-{tenant_id}");
        let account_id = Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, 1)")
            .bind(&tenant_id)
            .bind(&tenant_external_id)
            .execute(&database.pool)
            .await
            .expect("token tenant fixture");
        sqlx::query(
            "INSERT INTO upstream_accounts (id, tenant_id, name, driver, auth_kind, config_json, status, credential_generation, created_at, updated_at) VALUES ($1, $2, 'token-account', 'http-json', 'api_key', '{}', 'active', 1, 1, 1)",
        )
        .bind(&account_id)
        .bind(&tenant_id)
        .execute(&database.pool)
        .await
        .expect("token account fixture");

        sqlx::query(
            "INSERT INTO request_stats_facts (request_id, tenant_id, key_id, created_at, model, protocol, status_class, error_code, upstream_account_id, model_route_id, duration_ms, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, currency, cost_micros) VALUES ($1, $2, 'edge-key', 100, 'edge-model', 'openai', 'success', '', $3, '', 25, 10, 7, 2, 3, 'USD', 10), ($4, $2, 'edge-key', $5, 'edge-model', 'openai', 'success', '', $3, '', 25, 23, 29, 5, 7, 'USD', 10)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(&tenant_id)
        .bind(&account_id)
        .bind(Uuid::now_v7().to_string())
        .bind(2 * HOUR_MILLIS)
        .execute(&database.pool)
        .await
        .expect("edge token facts");
        sqlx::query(
            "INSERT INTO usage_analysis_hourly (tenant_id, key_id, hour_bucket, source_kind, model, protocol, status_class, error_code, upstream_account_id, model_route_id, service_tier, currency, requests, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, generation_units, duration_count, duration_sum_ms, duration_bucket_0, duration_bucket_1, duration_bucket_2, duration_bucket_3, duration_bucket_4, duration_bucket_5, duration_bucket_6, duration_bucket_7, duration_bucket_8, duration_bucket_9, duration_bucket_10, duration_bucket_11, cost_micros) VALUES ($1, 'rollup-key', 1, 'request', 'rollup-model', 'openai', 'success', '', $2, '', 'default', 'USD', 1, 11, 19, 13, 17, 0, 1, 25, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 10)",
        )
        .bind(&tenant_id)
        .bind(&account_id)
        .execute(&database.pool)
        .await
        .expect("complete hourly token rollup");

        let window = database
            .upstream_account_availability(
                &tenant_external_id,
                UpstreamAccountAvailabilityFilter {
                    from_created_at: 1,
                    to_created_at: 2 * HOUR_MILLIS,
                },
            )
            .await
            .expect("token availability window");
        let metrics = &window
            .accounts
            .first()
            .expect("token account result")
            .metrics;
        assert_eq!(window.accounts.len(), 1);
        assert_eq!(window.accounts[0].upstream_account_id, account_id);
        assert_eq!(metrics.requests, 3);
        assert_eq!(metrics.total_tokens, 129);
        let cache_rate = metrics.cache_rate.expect("non-zero token denominator");
        assert!((cache_rate - (20.0 / 74.0)).abs() < 1e-12);
    }

    async fn assert_tenant_window(database: &Database) {
        let tenant_id = Uuid::now_v7().to_string();
        let other_tenant_id = Uuid::now_v7().to_string();
        let tenant_external_id = format!("availability-{tenant_id}");
        let other_tenant_external_id = format!("availability-{other_tenant_id}");
        sqlx::query(
            "INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, 1), ($3, $4, 1)",
        )
        .bind(&tenant_id)
        .bind(&tenant_external_id)
        .bind(&other_tenant_id)
        .bind(&other_tenant_external_id)
        .execute(&database.pool)
        .await
        .expect("tenant fixtures");
        let account_a = Uuid::now_v7().to_string();
        let account_b = Uuid::now_v7().to_string();
        let account_zero = Uuid::now_v7().to_string();
        let foreign_account = Uuid::now_v7().to_string();
        for (id, account_tenant, name) in [
            (&account_a, &tenant_id, "account-a"),
            (&account_b, &tenant_id, "account-b"),
            (&account_zero, &tenant_id, "account-zero"),
            (&foreign_account, &other_tenant_id, "foreign-account"),
        ] {
            sqlx::query(
                "INSERT INTO upstream_accounts (id, tenant_id, name, driver, auth_kind, config_json, status, credential_generation, created_at, updated_at) VALUES ($1, $2, $3, 'http-json', 'api_key', '{}', 'active', 1, 1, 1)",
            )
            .bind(id)
            .bind(account_tenant)
            .bind(name)
            .execute(&database.pool)
            .await
            .expect("upstream account fixture");
        }

        for created_at in 100_i64..=105 {
            sqlx::query(
                "INSERT INTO request_stats_facts (request_id, tenant_id, key_id, created_at, model, protocol, status_class, error_code, upstream_account_id, model_route_id, duration_ms, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, currency, cost_micros) VALUES ($1, $2, 'key-a', $3, 'model-a', 'openai', 'success', '', $4, 'route-a', 25, 10, 5, 4, 2, 'USD', 10)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(&tenant_id)
            .bind(created_at)
            .bind(&account_a)
            .execute(&database.pool)
            .await
            .expect("request terminal fixture");
        }
        sqlx::query(
            "INSERT INTO generation_stats_facts (job_id, tenant_id, key_id, created_at, model, status_class, error_code, upstream_account_id, duration_ms, cost_micros, billed_units, currency, modality, billing_unit, model_route_id) VALUES ($1, $2, 'key-a', 106, 'image-a', 'failure', 'upstream_failed', $3, 40, 20, 1, 'USD', 'image', 'image', 'route-a')",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(&tenant_id)
        .bind(&account_a)
        .execute(&database.pool)
        .await
        .expect("generation terminal fixture");
        let request_b = Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO request_stats_facts (request_id, tenant_id, key_id, created_at, model, protocol, status_class, error_code, upstream_account_id, model_route_id, duration_ms, input_tokens, output_tokens, currency, cost_micros) VALUES ($1, $2, 'key-b', 200, 'model-b', 'openai', 'success', '', $3, 'route-b', 50, 1, 1, 'USD', 30)",
        )
        .bind(&request_b)
        .bind(&tenant_id)
        .bind(&account_b)
        .execute(&database.pool)
        .await
        .expect("second-account terminal fixture");
        sqlx::query(
            "INSERT INTO request_stats_facts (request_id, tenant_id, key_id, created_at, model, protocol, status_class, error_code, upstream_account_id, model_route_id, duration_ms, input_tokens, output_tokens, currency, cost_micros) VALUES ($1, $2, 'key-foreign', 300, 'model-foreign', 'openai', 'success', '', $3, 'route-foreign', 10, 1, 1, 'USD', 10)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(&other_tenant_id)
        .bind(&foreign_account)
        .execute(&database.pool)
        .await
        .expect("foreign terminal fixture");

        let window = database
            .upstream_account_availability(
                &tenant_external_id,
                UpstreamAccountAvailabilityFilter {
                    from_created_at: 0,
                    to_created_at: 999,
                },
            )
            .await
            .expect("availability window");
        assert_eq!(window.accounts.len(), 3);
        assert!(
            window
                .accounts
                .iter()
                .all(|account| account.upstream_account_id != foreign_account)
        );

        let first = window
            .accounts
            .iter()
            .find(|account| account.upstream_account_id == account_a)
            .expect("first account is present");
        assert_eq!(first.metrics.requests, 7);
        assert_eq!(first.metrics.successful_requests, 6);
        assert_eq!(first.metrics.failed_requests, 1);
        assert_eq!(first.metrics.total_tokens, 90);
        assert_eq!(first.metrics.cache_rate, Some(0.4));
        assert_eq!(first.terminal_outcomes.len(), 5);
        assert_eq!(first.terminal_outcomes[0].created_at, 106);
        assert_eq!(first.terminal_outcomes[0].source, "generation");
        assert_eq!(first.terminal_outcomes[4].created_at, 102);
        assert_eq!(
            first.terminal_outcomes[0].error_code.as_deref(),
            Some("upstream_failed")
        );

        let second = window
            .accounts
            .iter()
            .find(|account| account.upstream_account_id == account_b)
            .expect("second account is present");
        assert_eq!(second.metrics.requests, 1);
        assert_eq!(second.metrics.total_tokens, 2);
        assert_eq!(second.metrics.cache_rate, Some(0.0));
        assert_eq!(second.terminal_outcomes.len(), 1);
        assert_eq!(second.terminal_outcomes[0].id, request_b);

        let zero = window
            .accounts
            .iter()
            .find(|account| account.upstream_account_id == account_zero)
            .expect("unobserved account is present");
        assert_eq!(zero.metrics.requests, 0);
        assert_eq!(zero.metrics.successful_requests, 0);
        assert_eq!(zero.metrics.failed_requests, 0);
        assert_eq!(zero.metrics.total_tokens, 0);
        assert!(zero.metrics.cache_rate.is_none());
        assert!(zero.metrics.avg_duration_ms.is_none());
        assert!(zero.metrics.p95_duration_ms.is_none());
        assert!(zero.metrics.costs.is_empty());
        assert!(zero.terminal_outcomes.is_empty());
    }
}
