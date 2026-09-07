use std::collections::BTreeMap;

use sqlx::{AnyConnection, Row, any::AnyRow};
use uuid::Uuid;

use super::{
    AppError, Database, DatabaseBackend, MAX_STATS_RANGE_MILLIS, micros_to_decimal_string,
    unix_millis,
};
use crate::model::{
    MonitoringFreshness, MonitoringHealth, MonitoringMetrics, MonitoringTerminalOutcome,
    MonitoringUpstreamModel, OperatorMonitoringSnapshot, UsageAnalysisCost,
};

#[path = "monitoring_snapshot_sql.rs"]
mod monitoring_snapshot_sql;
use monitoring_snapshot_sql::{
    monitoring_freshness_sql, monitoring_snapshot_sql, monitoring_terminal_outcomes_sql,
};

const HOUR_MILLIS: i64 = 3_600_000;
const DAY_MILLIS: i64 = 86_400_000;
const HOUR_RANGE_LIMIT: i64 = 31 * DAY_MILLIS;
const HEALTH_VERSION: &str = "upstream_breaker_v1";

/// An operator-selected scope. Tenant scope is a stable external identifier at
/// the API boundary and is resolved once to the database tenant key before
/// any fact query begins.
#[derive(Clone, Debug)]
pub enum MonitoringScope {
    Tenant(String),
    Global,
}

impl MonitoringScope {
    pub fn scope_name(&self) -> &'static str {
        match self {
            Self::Tenant(_) => "tenant",
            Self::Global => "global",
        }
    }

    fn tenant_external_id(&self) -> Option<&str> {
        match self {
            Self::Tenant(value) => Some(value),
            Self::Global => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct MonitoringSnapshotFilter {
    pub from_created_at: i64,
    pub to_created_at: i64,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum MonitoringGranularity {
    Hour,
    Day,
}

impl MonitoringGranularity {
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

struct MonitoringRange {
    from_created_at: i64,
    to_created_at: i64,
    granularity: MonitoringGranularity,
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
    /// Preserve the usage-analysis inclusive-window contract exactly: only
    /// complete buckets hit rollups, while at most two partial edge ranges hit
    /// terminal facts.
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
    duration_count: i64,
    duration_sum_ms: i64,
    duration_buckets: [i64; 12],
    costs: BTreeMap<String, i64>,
}

impl MetricsAccumulator {
    fn accumulate(&mut self, row: &AnyRow) -> Result<(), AppError> {
        self.requests = self.requests.saturating_add(row.try_get("requests")?);
        self.successful_requests = self
            .successful_requests
            .saturating_add(row.try_get("successful_requests")?);
        self.failed_requests = self
            .failed_requests
            .saturating_add(row.try_get("failed_requests")?);
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
        let currency: String = row.try_get("currency")?;
        if currency.is_empty() {
            return Err(AppError::Internal);
        }
        let cost = self.costs.entry(currency).or_default();
        *cost = cost.saturating_add(row.try_get("cost_micros")?);
        Ok(())
    }

    fn finish(self) -> MonitoringMetrics {
        MonitoringMetrics {
            requests: self.requests,
            successful_requests: self.successful_requests,
            failed_requests: self.failed_requests,
            avg_duration_ms: (self.duration_count > 0)
                .then(|| self.duration_sum_ms as f64 / self.duration_count as f64),
            p95_duration_ms: approximate_quantile(95, self.duration_count, &self.duration_buckets),
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
    /// Read one bounded operator monitoring snapshot. The source is limited to
    /// 93 days, uses hourly rollups through 31 days and daily rollups after
    /// that, and only scans terminal facts in the two incomplete edge buckets.
    pub async fn operator_monitoring_snapshot(
        &self,
        scope: MonitoringScope,
        filter: MonitoringSnapshotFilter,
    ) -> Result<OperatorMonitoringSnapshot, AppError> {
        let range = validate_monitoring_range(filter)?;
        let generated_at = unix_millis();
        let tenant_external_id = scope.tenant_external_id().map(str::to_owned);
        let tenant_id = match tenant_external_id.as_deref() {
            Some(external_id) => sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
                .bind(external_id)
                .fetch_optional(&self.pool)
                .await?
                .map(|row| row.try_get::<String, _>("id"))
                .transpose()?
                // Keep an unknown tenant a strictly empty tenant scope rather
                // than silently widening it into the global snapshot.
                .unwrap_or_else(|| Uuid::nil().to_string()),
            None => String::new(),
        };
        let plan = BucketPlan::new(
            range.from_created_at,
            range.to_created_at,
            range.granularity.bucket_millis(),
        );
        let mut snapshot = self.pool.begin().await?;
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *snapshot)
                .await?;
        }

        let aggregate_sql = monitoring_snapshot_sql(&scope, range.granularity);
        let aggregate_rows = match &scope {
            MonitoringScope::Tenant(_) => {
                sqlx::query(sqlx::AssertSqlSafe(aggregate_sql))
                    .bind(&tenant_id)
                    .bind(plan.rollup_from_bucket)
                    .bind(plan.rollup_to_bucket)
                    .bind(plan.left_from_created_at)
                    .bind(plan.left_to_created_at)
                    .bind(plan.right_from_created_at)
                    .bind(plan.right_to_created_at)
                    .fetch_all(&mut *snapshot)
                    .await?
            }
            MonitoringScope::Global => {
                sqlx::query(sqlx::AssertSqlSafe(aggregate_sql))
                    .bind(plan.rollup_from_bucket)
                    .bind(plan.rollup_to_bucket)
                    .bind(plan.left_from_created_at)
                    .bind(plan.left_to_created_at)
                    .bind(plan.right_from_created_at)
                    .bind(plan.right_to_created_at)
                    .fetch_all(&mut *snapshot)
                    .await?
            }
        };
        let mut summary = MetricsAccumulator::default();
        let mut top = BTreeMap::<(String, String), MetricsAccumulator>::new();
        for row in aggregate_rows {
            let kind: String = row.try_get("kind")?;
            let upstream_account_id: String = row.try_get("upstream_account_id")?;
            let model: String = row.try_get("model")?;
            match kind.as_str() {
                "summary" => summary.accumulate(&row)?,
                "top" if !upstream_account_id.is_empty() && !model.is_empty() => top
                    .entry((upstream_account_id, model))
                    .or_default()
                    .accumulate(&row)?,
                _ => return Err(AppError::Internal),
            }
        }
        let summary = summary.finish();
        let latest_terminal_created_at = latest_terminal_created_at(
            &mut *snapshot,
            &scope,
            &tenant_id,
            range.from_created_at,
            range.to_created_at,
        )
        .await?;
        let freshness = MonitoringFreshness {
            latest_terminal_created_at,
            age_millis: latest_terminal_created_at
                .map(|created_at| generated_at.saturating_sub(created_at)),
        };

        let mut top_upstream_models = Vec::with_capacity(top.len());
        for ((upstream_account_id, model), accumulator) in top {
            let (upstream_name, health) =
                upstream_health(&mut *snapshot, &upstream_account_id, generated_at).await?;
            let terminal_outcomes = terminal_outcomes(
                &mut *snapshot,
                &scope,
                &tenant_id,
                &upstream_account_id,
                &model,
                range.from_created_at,
                range.to_created_at,
            )
            .await?;
            top_upstream_models.push(MonitoringUpstreamModel {
                upstream_account_id,
                upstream_name,
                model,
                metrics: accumulator.finish(),
                health,
                terminal_outcomes,
            });
        }
        top_upstream_models.sort_by(|left, right| {
            right
                .metrics
                .requests
                .cmp(&left.metrics.requests)
                .then_with(|| left.upstream_account_id.cmp(&right.upstream_account_id))
                .then_with(|| left.model.cmp(&right.model))
        });
        let health = aggregate_health(&summary, &top_upstream_models);
        snapshot.commit().await?;

        Ok(OperatorMonitoringSnapshot {
            contract_version: "v1".to_owned(),
            generated_at,
            scope: scope.scope_name().to_owned(),
            tenant_external_id,
            from_created_at: range.from_created_at,
            to_created_at: range.to_created_at,
            granularity: range.granularity.as_str().to_owned(),
            latency_is_approximate: true,
            latency_method: "fixed_histogram_upper_bound_capped_60000ms".to_owned(),
            summary,
            freshness,
            health,
            top_upstream_models,
        })
    }
}

fn validate_monitoring_range(
    filter: MonitoringSnapshotFilter,
) -> Result<MonitoringRange, AppError> {
    if filter.from_created_at < 0
        || filter.to_created_at < 0
        || filter.from_created_at > filter.to_created_at
    {
        return Err(AppError::BadRequest(
            "monitoring snapshot requires a valid non-negative inclusive time window".into(),
        ));
    }
    let width = filter.to_created_at.saturating_sub(filter.from_created_at);
    if width > MAX_STATS_RANGE_MILLIS {
        return Err(AppError::BadRequest(
            "monitoring snapshot range must not exceed 93 days".into(),
        ));
    }
    Ok(MonitoringRange {
        from_created_at: filter.from_created_at,
        to_created_at: filter.to_created_at,
        granularity: if width <= HOUR_RANGE_LIMIT {
            MonitoringGranularity::Hour
        } else {
            MonitoringGranularity::Day
        },
    })
}

async fn latest_terminal_created_at(
    connection: &mut AnyConnection,
    scope: &MonitoringScope,
    tenant_id: &str,
    from_created_at: i64,
    to_created_at: i64,
) -> Result<Option<i64>, AppError> {
    let sql = monitoring_freshness_sql(scope);
    let row = match scope {
        MonitoringScope::Tenant(_) => {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(tenant_id)
                .bind(from_created_at)
                .bind(to_created_at)
                .fetch_optional(connection)
                .await?
        }
        MonitoringScope::Global => {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(from_created_at)
                .bind(to_created_at)
                .fetch_optional(connection)
                .await?
        }
    };
    row.map(|row| row.try_get("created_at"))
        .transpose()
        .map_err(Into::into)
}

async fn terminal_outcomes(
    connection: &mut AnyConnection,
    scope: &MonitoringScope,
    tenant_id: &str,
    upstream_account_id: &str,
    model: &str,
    from_created_at: i64,
    to_created_at: i64,
) -> Result<Vec<MonitoringTerminalOutcome>, AppError> {
    let sql = monitoring_terminal_outcomes_sql(scope);
    let rows = match scope {
        MonitoringScope::Tenant(_) => {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(tenant_id)
                .bind(upstream_account_id)
                .bind(model)
                .bind(from_created_at)
                .bind(to_created_at)
                .fetch_all(connection)
                .await?
        }
        MonitoringScope::Global => {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(upstream_account_id)
                .bind(model)
                .bind(from_created_at)
                .bind(to_created_at)
                .fetch_all(connection)
                .await?
        }
    };
    rows.into_iter()
        .map(|row| {
            let source: String = row.try_get("source")?;
            let status: String = row.try_get("status_class")?;
            if !matches!(source.as_str(), "request" | "generation")
                || !matches!(status.as_str(), "success" | "failure")
            {
                return Err(AppError::Internal);
            }
            let error_code: String = row.try_get("error_code")?;
            Ok(MonitoringTerminalOutcome {
                id: row.try_get("id")?,
                source,
                created_at: row.try_get("created_at")?,
                status,
                duration_ms: row.try_get("duration_ms")?,
                error_code: (!error_code.is_empty()).then_some(error_code),
            })
        })
        .collect()
}

async fn upstream_health(
    connection: &mut AnyConnection,
    upstream_account_id: &str,
    generated_at: i64,
) -> Result<(String, MonitoringHealth), AppError> {
    let row = sqlx::query(
        "SELECT account.name, account.status, account.credential_generation, \
                health.credential_generation AS health_generation, \
                health.consecutive_failures, health.cooldown_until, health.updated_at \
           FROM upstream_accounts account \
      LEFT JOIN upstream_account_health health \
             ON health.upstream_account_id = account.id \
          WHERE account.id = $1",
    )
    .bind(upstream_account_id)
    .fetch_optional(connection)
    .await?;
    let Some(row) = row else {
        return Ok((upstream_account_id.to_owned(), unknown_health()));
    };
    let name: String = row.try_get("name")?;
    let account_status: String = row.try_get("status")?;
    let credential_generation: i64 = row.try_get("credential_generation")?;
    let health_generation: Option<i64> = row.try_get("health_generation")?;
    let consecutive_failures: Option<i64> = row.try_get("consecutive_failures")?;
    let cooldown_until: Option<i64> = row.try_get("cooldown_until")?;
    let observed_at: Option<i64> = row.try_get("updated_at")?;
    let status = if account_status != "active" {
        "unhealthy"
    } else if health_generation == Some(credential_generation) {
        if cooldown_until.unwrap_or_default() > generated_at {
            "unhealthy"
        } else if consecutive_failures.unwrap_or_default() > 0 {
            "degraded"
        } else {
            "healthy"
        }
    } else {
        // A previous credential generation cannot poison the current stable
        // account's health. Absence of a current breaker row is healthy only
        // because this caller already observed terminal traffic for the pair.
        "healthy"
    };
    Ok((
        name,
        MonitoringHealth {
            version: HEALTH_VERSION.to_owned(),
            status: status.to_owned(),
            observed_at,
        },
    ))
}

fn unknown_health() -> MonitoringHealth {
    MonitoringHealth {
        version: HEALTH_VERSION.to_owned(),
        status: "unknown".to_owned(),
        observed_at: None,
    }
}

fn aggregate_health(
    summary: &MonitoringMetrics,
    upstream_models: &[MonitoringUpstreamModel],
) -> MonitoringHealth {
    if summary.requests == 0 || upstream_models.is_empty() {
        // There is no current health conclusion to make from a zero-traffic
        // window (or terminal traffic that had no stable upstream identity).
        return unknown_health();
    }
    let status = if upstream_models
        .iter()
        .any(|value| value.health.status == "unhealthy")
    {
        "unhealthy"
    } else if upstream_models
        .iter()
        .any(|value| value.health.status == "degraded")
    {
        "degraded"
    } else if upstream_models
        .iter()
        .any(|value| value.health.status == "healthy")
    {
        "healthy"
    } else {
        "unknown"
    };
    MonitoringHealth {
        version: HEALTH_VERSION.to_owned(),
        status: status.to_owned(),
        observed_at: upstream_models
            .iter()
            .filter_map(|value| value.health.observed_at)
            .max(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitoring_window_preserves_the_hour_and_day_retention_fences() {
        let hour = validate_monitoring_range(MonitoringSnapshotFilter {
            from_created_at: 0,
            to_created_at: HOUR_RANGE_LIMIT,
        })
        .unwrap();
        assert!(matches!(hour.granularity, MonitoringGranularity::Hour));
        let day = validate_monitoring_range(MonitoringSnapshotFilter {
            from_created_at: 0,
            to_created_at: HOUR_RANGE_LIMIT + 1,
        })
        .unwrap();
        assert!(matches!(day.granularity, MonitoringGranularity::Day));
        assert!(
            validate_monitoring_range(MonitoringSnapshotFilter {
                from_created_at: 0,
                to_created_at: MAX_STATS_RANGE_MILLIS + 1,
            })
            .is_err()
        );
    }

    #[test]
    fn zero_traffic_health_is_versioned_unknown() {
        let health = aggregate_health(&MonitoringMetrics::default(), &[]);
        assert_eq!(health.version, HEALTH_VERSION);
        assert_eq!(health.status, "unknown");
    }

    #[test]
    fn bucket_plan_uses_only_two_exact_terminal_fact_edges() {
        let now = 123 * HOUR_MILLIS + 456;
        let plan = BucketPlan::new(now - DAY_MILLIS, now, HOUR_MILLIS);
        assert_eq!(plan.rollup_from_bucket, 100);
        assert_eq!(plan.rollup_to_bucket, 123);
        assert_eq!(plan.left_from_created_at, now - DAY_MILLIS);
        assert_eq!(plan.left_to_created_at, 100 * HOUR_MILLIS - 1);
        assert_eq!(plan.right_from_created_at, 123 * HOUR_MILLIS);
        assert_eq!(plan.right_to_created_at, now);
    }
}
