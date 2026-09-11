use std::collections::{BTreeMap, BTreeSet};

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
    MonitoringTerminalBatchDialect, monitoring_freshness_sql, monitoring_snapshot_sql,
    monitoring_terminal_outcomes_batch_sql, monitoring_upstream_health_batch_sql,
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

struct MonitoringBatchContext<'a> {
    backend: DatabaseBackend,
    scope: &'a MonitoringScope,
    tenant_id: &'a str,
    from_created_at: i64,
    to_created_at: i64,
}

#[derive(Default)]
struct MonitoringStatementCounter {
    // Explicit statements issued by this module. Transaction protocol
    // BEGIN/COMMIT messages are deliberately outside this query budget.
    count: usize,
}

impl MonitoringStatementCounter {
    fn record(&mut self) {
        self.count = self.count.saturating_add(1);
    }
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
        let mut statement_counter = MonitoringStatementCounter::default();
        self.operator_monitoring_snapshot_inner(scope, filter, &mut statement_counter)
            .await
    }

    async fn operator_monitoring_snapshot_inner(
        &self,
        scope: MonitoringScope,
        filter: MonitoringSnapshotFilter,
        statement_counter: &mut MonitoringStatementCounter,
    ) -> Result<OperatorMonitoringSnapshot, AppError> {
        let range = validate_monitoring_range(filter)?;
        let generated_at = unix_millis();
        let tenant_external_id = scope.tenant_external_id().map(str::to_owned);
        let tenant_id = match tenant_external_id.as_deref() {
            Some(external_id) => {
                statement_counter.record();
                sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
                    .bind(external_id)
                    .fetch_optional(&self.pool)
                    .await?
                    .map(|row| row.try_get::<String, _>("id"))
                    .transpose()?
                    // Keep an unknown tenant a strictly empty tenant scope rather
                    // than silently widening it into the global snapshot.
                    .unwrap_or_else(|| Uuid::nil().to_string())
            }
            None => String::new(),
        };
        let plan = BucketPlan::new(
            range.from_created_at,
            range.to_created_at,
            range.granularity.bucket_millis(),
        );
        let mut snapshot = self.pool.begin().await?;
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            statement_counter.record();
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *snapshot)
                .await?;
        }

        let aggregate_sql = monitoring_snapshot_sql(&scope, range.granularity);
        statement_counter.record();
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
            &mut snapshot,
            &scope,
            &tenant_id,
            range.from_created_at,
            range.to_created_at,
            statement_counter,
        )
        .await?;
        let freshness = MonitoringFreshness {
            latest_terminal_created_at,
            age_millis: latest_terminal_created_at
                .map(|created_at| generated_at.saturating_sub(created_at)),
        };

        let pairs = top.keys().cloned().collect::<Vec<_>>();
        let upstream_account_ids = pairs
            .iter()
            .map(|(upstream_account_id, _)| upstream_account_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let upstream_health = upstream_health_batch(
            &mut snapshot,
            &upstream_account_ids,
            generated_at,
            statement_counter,
        )
        .await?;
        let mut outcomes = terminal_outcomes_batch(
            &mut snapshot,
            MonitoringBatchContext {
                backend: self.backend,
                scope: &scope,
                tenant_id: &tenant_id,
                from_created_at: range.from_created_at,
                to_created_at: range.to_created_at,
            },
            &pairs,
            statement_counter,
        )
        .await?;
        let mut top_upstream_models = Vec::with_capacity(top.len());
        for ((upstream_account_id, model), accumulator) in top {
            let (upstream_name, health) = upstream_health
                .get(&upstream_account_id)
                .cloned()
                .unwrap_or_else(|| (upstream_account_id.clone(), unknown_health()));
            let terminal_outcomes = outcomes
                .remove(&(upstream_account_id.clone(), model.clone()))
                .unwrap_or_default();
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
    statement_counter: &mut MonitoringStatementCounter,
) -> Result<Option<i64>, AppError> {
    let sql = monitoring_freshness_sql(scope);
    statement_counter.record();
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

async fn terminal_outcomes_batch(
    connection: &mut AnyConnection,
    context: MonitoringBatchContext<'_>,
    pairs: &[(String, String)],
    statement_counter: &mut MonitoringStatementCounter,
) -> Result<BTreeMap<(String, String), Vec<MonitoringTerminalOutcome>>, AppError> {
    let mut outcomes = pairs
        .iter()
        .cloned()
        .map(|pair| (pair, Vec::new()))
        .collect::<BTreeMap<_, _>>();
    if pairs.is_empty() {
        return Ok(outcomes);
    }
    let dialect = match context.backend {
        DatabaseBackend::PostgreSql => MonitoringTerminalBatchDialect::PostgreSql,
        DatabaseBackend::Sqlite => MonitoringTerminalBatchDialect::Sqlite,
    };
    let sql = monitoring_terminal_outcomes_batch_sql(context.scope, pairs.len(), dialect)
        .ok_or(AppError::Internal)?;
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for (upstream_account_id, model) in pairs {
        query = query.bind(upstream_account_id).bind(model);
    }
    if matches!(context.scope, MonitoringScope::Tenant(_)) {
        query = query.bind(context.tenant_id);
    }
    statement_counter.record();
    let rows = query
        .bind(context.from_created_at)
        .bind(context.to_created_at)
        .fetch_all(connection)
        .await?;
    for row in rows {
        let upstream_account_id: String = row.try_get("upstream_account_id")?;
        let model: String = row.try_get("model")?;
        let pair = (upstream_account_id, model);
        let Some(pair_outcomes) = outcomes.get_mut(&pair) else {
            return Err(AppError::Internal);
        };
        let source: String = row.try_get("source")?;
        let status: String = row.try_get("status_class")?;
        if !matches!(source.as_str(), "request" | "generation")
            || !matches!(status.as_str(), "success" | "failure")
        {
            return Err(AppError::Internal);
        }
        let error_code: String = row.try_get("error_code")?;
        pair_outcomes.push(MonitoringTerminalOutcome {
            id: row.try_get("id")?,
            source,
            created_at: row.try_get("created_at")?,
            status,
            duration_ms: row.try_get("duration_ms")?,
            error_code: (!error_code.is_empty()).then_some(error_code),
        });
    }
    Ok(outcomes)
}

async fn upstream_health_batch(
    connection: &mut AnyConnection,
    upstream_account_ids: &[String],
    generated_at: i64,
    statement_counter: &mut MonitoringStatementCounter,
) -> Result<BTreeMap<String, (String, MonitoringHealth)>, AppError> {
    if upstream_account_ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let sql = monitoring_upstream_health_batch_sql(upstream_account_ids.len())
        .ok_or(AppError::Internal)?;
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for upstream_account_id in upstream_account_ids {
        query = query.bind(upstream_account_id);
    }
    statement_counter.record();
    let rows = query.fetch_all(connection).await?;
    let mut health = BTreeMap::new();
    for row in rows {
        let upstream_account_id: String = row.try_get("upstream_account_id")?;
        let value = upstream_health_from_row(&row, generated_at)?;
        if health.insert(upstream_account_id, value).is_some() {
            return Err(AppError::Internal);
        }
    }
    Ok(health)
}

fn upstream_health_from_row(
    row: &AnyRow,
    generated_at: i64,
) -> Result<(String, MonitoringHealth), AppError> {
    let name: String = row.try_get("name")?;
    let account_status: Option<String> = row.try_get("status")?;
    let credential_generation: Option<i64> = row.try_get("credential_generation")?;
    let health_generation: Option<i64> = row.try_get("health_generation")?;
    let consecutive_failures: Option<i64> = row.try_get("consecutive_failures")?;
    let cooldown_until: Option<i64> = row.try_get("cooldown_until")?;
    let observed_at: Option<i64> = row.try_get("updated_at")?;
    let Some(account_status) = account_status else {
        return Ok((name, unknown_health()));
    };
    let Some(credential_generation) = credential_generation else {
        return Err(AppError::Internal);
    };
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
    use sqlx::{Connection, PgConnection};

    const FIXTURE_FROM: i64 = 1_000;
    const FIXTURE_TO: i64 = 2_000;

    struct PostgresMonitoringFixture {
        tenant_id: String,
        tenant_external_id: String,
        pairs: Vec<(String, String)>,
    }

    async fn seed_postgres_monitoring_fixture(
        database: &Database,
        pair_count: usize,
    ) -> PostgresMonitoringFixture {
        let fixture_id = Uuid::now_v7();
        let tenant_id = Uuid::now_v7().to_string();
        let tenant_external_id = format!("monitoring-batch-{fixture_id}");
        sqlx::query("INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, 1)")
            .bind(&tenant_id)
            .bind(&tenant_external_id)
            .execute(&database.pool)
            .await
            .unwrap();
        let mut pairs = Vec::with_capacity(pair_count);
        for index in 0..pair_count {
            let upstream_account_id = Uuid::now_v7().to_string();
            let model = format!("monitoring-model-{index}");
            let name = format!("monitoring-upstream-{index}");
            let account_status = if index == 2 { "disabled" } else { "active" };
            sqlx::query(
                "INSERT INTO upstream_accounts (
                     id, tenant_id, name, driver, auth_kind, config_json, status,
                     credential_generation, created_at, updated_at
                 ) VALUES ($1, $2, $3, 'http-json', 'none', '{}', $4, 2, 1, 1)",
            )
            .bind(&upstream_account_id)
            .bind(&tenant_id)
            .bind(&name)
            .bind(account_status)
            .execute(&database.pool)
            .await
            .unwrap();
            let (health_generation, consecutive_failures, cooldown_until) = match index {
                1 => (2_i64, 1_i64, 0_i64),
                // A stale breaker generation must not poison the rotated
                // current credential, even if the old generation was open.
                3 => (1_i64, 9_i64, i64::MAX),
                _ => (2_i64, 0_i64, 0_i64),
            };
            sqlx::query(
                "INSERT INTO upstream_account_health (
                     upstream_account_id, consecutive_failures, cooldown_until,
                     probe_lease_until, last_failure_kind, updated_at,
                     credential_generation
                 ) VALUES ($1, $2, $3, 0, '', 700, $4)",
            )
            .bind(&upstream_account_id)
            .bind(consecutive_failures)
            .bind(cooldown_until)
            .bind(health_generation)
            .execute(&database.pool)
            .await
            .unwrap();

            if index == 4 {
                sqlx::query(
                    "INSERT INTO deleted_upstream_account_snapshots (
                         upstream_account_id, tenant_id, name, driver, auth_kind,
                         credential_generation, created_at, deleted_at
                     ) VALUES ($1, $2, $3, 'http-json', 'none', 2, 1, 2)",
                )
                .bind(&upstream_account_id)
                .bind(&tenant_id)
                .bind(&name)
                .execute(&database.pool)
                .await
                .unwrap();
                sqlx::query("DELETE FROM upstream_accounts WHERE id = $1")
                    .bind(&upstream_account_id)
                    .execute(&database.pool)
                    .await
                    .unwrap();
            }

            let fact_prefix = format!("{fixture_id}-{index}");
            sqlx::query(
                "INSERT INTO request_stats_facts (
                     request_id, tenant_id, key_id, created_at, model, protocol,
                     status_class, error_code, upstream_account_id, model_route_id,
                     duration_ms, input_tokens, output_tokens, cached_input_tokens,
                     cache_write_tokens, service_tier, currency, cost_micros
                 )
                 SELECT $1 || '-request-' || n::text, $2, $1 || '-key',
                        1100 + n * 20, $3, 'openai',
                        CASE WHEN n % 2 = 0 THEN 'success' ELSE 'failure' END,
                        CASE WHEN n % 2 = 0 THEN '' ELSE 'request_failure' END,
                        $4, '', 50 + n, 10, 5, 0, 0, 'default', 'USD', 10
                   FROM generate_series(0, 6) AS fixture(n)",
            )
            .bind(&fact_prefix)
            .bind(&tenant_id)
            .bind(&model)
            .bind(&upstream_account_id)
            .execute(&database.pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO generation_stats_facts (
                     job_id, tenant_id, key_id, created_at, model, status_class,
                     error_code, upstream_account_id, duration_ms, cost_micros,
                     billed_units, currency
                 )
                 SELECT $1 || '-generation-' || n::text, $2, $1 || '-key',
                        1110 + n * 20, $3,
                        CASE WHEN n % 2 = 0 THEN 'failure' ELSE 'success' END,
                        CASE WHEN n % 2 = 0 THEN 'generation_failure' ELSE '' END,
                        $4, 70 + n, 20, 1, 'USD'
                   FROM generate_series(0, 6) AS fixture(n)",
            )
            .bind(&fact_prefix)
            .bind(&tenant_id)
            .bind(&model)
            .bind(&upstream_account_id)
            .execute(&database.pool)
            .await
            .unwrap();
            pairs.push((upstream_account_id, model));
        }
        PostgresMonitoringFixture {
            tenant_id,
            tenant_external_id,
            pairs,
        }
    }

    type OutcomeSignature = (String, String, i64, String, i64, Option<String>);

    fn outcome_signature(outcome: &MonitoringTerminalOutcome) -> OutcomeSignature {
        (
            outcome.id.clone(),
            outcome.source.clone(),
            outcome.created_at,
            outcome.status.clone(),
            outcome.duration_ms,
            outcome.error_code.clone(),
        )
    }

    async fn reference_terminal_outcomes(
        connection: &mut AnyConnection,
        tenant_id: &str,
        upstream_account_id: &str,
        model: &str,
    ) -> Vec<OutcomeSignature> {
        let rows = sqlx::query(
            "SELECT id, source, created_at, status_class, duration_ms, error_code
               FROM (
                     (SELECT f.request_id AS id, 'request' AS source, f.created_at,
                             f.status_class, f.duration_ms, f.error_code
                        FROM request_stats_facts f
                       WHERE f.tenant_id = $1 AND f.upstream_account_id = $2
                         AND f.model = $3 AND f.created_at >= $4 AND f.created_at <= $5
                       ORDER BY f.created_at DESC, f.request_id DESC
                       LIMIT 5)
                     UNION ALL
                     (SELECT f.job_id AS id, 'generation' AS source, f.created_at,
                             f.status_class, f.duration_ms, f.error_code
                        FROM generation_stats_facts f
                       WHERE f.tenant_id = $1 AND f.upstream_account_id = $2
                         AND f.model = $3 AND f.created_at >= $4 AND f.created_at <= $5
                       ORDER BY f.created_at DESC, f.job_id DESC
                       LIMIT 5)
               ) terminal
              ORDER BY created_at DESC, id DESC
              LIMIT 5",
        )
        .bind(tenant_id)
        .bind(upstream_account_id)
        .bind(model)
        .bind(FIXTURE_FROM)
        .bind(FIXTURE_TO)
        .fetch_all(connection)
        .await
        .unwrap();
        rows.into_iter()
            .map(|row| {
                let error_code: String = row.try_get("error_code").unwrap();
                (
                    row.try_get("id").unwrap(),
                    row.try_get("source").unwrap(),
                    row.try_get("created_at").unwrap(),
                    row.try_get("status_class").unwrap(),
                    row.try_get("duration_ms").unwrap(),
                    (!error_code.is_empty()).then_some(error_code),
                )
            })
            .collect()
    }

    fn collect_plan_index_names(plan: &serde_json::Value, names: &mut BTreeSet<String>) {
        if let Some(name) = plan.get("Index Name").and_then(serde_json::Value::as_str) {
            names.insert(name.to_owned());
        }
        if let Some(children) = plan.get("Plans").and_then(serde_json::Value::as_array) {
            for child in children {
                collect_plan_index_names(child, names);
            }
        }
    }

    #[tokio::test]
    async fn postgres_monitoring_batch_is_constant_statement_equivalent_and_index_bounded() {
        let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        let database = Database::connect_with_max(&database_url, 8).await.unwrap();
        database.migrate().await.unwrap();
        let one = seed_postgres_monitoring_fixture(&database, 1).await;
        let ten = seed_postgres_monitoring_fixture(&database, 10).await;
        let filter = MonitoringSnapshotFilter {
            from_created_at: FIXTURE_FROM,
            to_created_at: FIXTURE_TO,
        };

        let mut one_statement_count = MonitoringStatementCounter::default();
        let one_snapshot = database
            .operator_monitoring_snapshot_inner(
                MonitoringScope::Tenant(one.tenant_external_id),
                filter.clone(),
                &mut one_statement_count,
            )
            .await
            .unwrap();
        let mut ten_statement_count = MonitoringStatementCounter::default();
        let ten_snapshot = database
            .operator_monitoring_snapshot_inner(
                MonitoringScope::Tenant(ten.tenant_external_id.clone()),
                filter,
                &mut ten_statement_count,
            )
            .await
            .unwrap();
        assert_eq!(one_snapshot.top_upstream_models.len(), 1);
        assert_eq!(ten_snapshot.top_upstream_models.len(), 10);
        assert_eq!(one_statement_count.count, 6);
        assert_eq!(ten_statement_count.count, one_statement_count.count);

        let snapshot_by_pair = ten_snapshot
            .top_upstream_models
            .iter()
            .map(|value| {
                (
                    (value.upstream_account_id.clone(), value.model.clone()),
                    value,
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut reference_connection = database.pool.acquire().await.unwrap();
        for (index, pair) in ten.pairs.iter().enumerate() {
            let value = snapshot_by_pair.get(pair).unwrap();
            let expected = reference_terminal_outcomes(
                &mut reference_connection,
                &ten.tenant_id,
                &pair.0,
                &pair.1,
            )
            .await;
            let actual = value
                .terminal_outcomes
                .iter()
                .map(outcome_signature)
                .collect::<Vec<_>>();
            assert_eq!(
                actual, expected,
                "terminal outcome mismatch for pair {index}"
            );
            let expected_status = match index {
                1 => "degraded",
                2 => "unhealthy",
                4 => "unknown",
                _ => "healthy",
            };
            assert_eq!(value.health.status, expected_status);
            assert_eq!(value.upstream_name, format!("monitoring-upstream-{index}"));
        }
        drop(reference_connection);

        let terminal_sql = monitoring_terminal_outcomes_batch_sql(
            &MonitoringScope::Tenant(ten.tenant_external_id),
            ten.pairs.len(),
            MonitoringTerminalBatchDialect::PostgreSql,
        )
        .unwrap();
        let mut explain_connection = PgConnection::connect(&database_url).await.unwrap();
        let mut explain_transaction = explain_connection.begin().await.unwrap();
        sqlx::query("SET LOCAL enable_seqscan = off")
            .execute(&mut *explain_transaction)
            .await
            .unwrap();
        let mut explain_query = sqlx::query(sqlx::AssertSqlSafe(format!(
            "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {terminal_sql}"
        )));
        for (upstream_account_id, model) in &ten.pairs {
            explain_query = explain_query.bind(upstream_account_id).bind(model);
        }
        let explain_row = explain_query
            .bind(&ten.tenant_id)
            .bind(FIXTURE_FROM)
            .bind(FIXTURE_TO)
            .fetch_one(&mut *explain_transaction)
            .await
            .unwrap();
        let explain_json: String = explain_row.try_get_unchecked(0).unwrap();
        let explain: serde_json::Value = serde_json::from_str(&explain_json).unwrap();
        let root = &explain[0]["Plan"];
        let mut index_names = BTreeSet::new();
        collect_plan_index_names(root, &mut index_names);
        assert!(index_names.contains("request_stats_facts_monitoring_outcome_idx"));
        assert!(index_names.contains("generation_stats_facts_monitoring_outcome_idx"));
        let shared_read_blocks = root
            .get("Shared Read Blocks")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default();
        let shared_hit_blocks = root
            .get("Shared Hit Blocks")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default();
        assert!(
            shared_read_blocks <= 256,
            "shared reads: {shared_read_blocks}"
        );
        assert!(
            shared_read_blocks + shared_hit_blocks <= 1_024,
            "shared blocks: read={shared_read_blocks}, hit={shared_hit_blocks}"
        );
    }

    #[tokio::test]
    async fn sqlite_terminal_batch_bounds_large_history_with_monitoring_indexes() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("monitoring-batch.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let fixture_id = Uuid::now_v7().to_string();
        let tenant_id = Uuid::now_v7().to_string();
        let upstream_account_id = Uuid::now_v7().to_string();
        let model = "sqlite-large-history".to_owned();
        sqlx::query("INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, 1)")
            .bind(&tenant_id)
            .bind(format!("sqlite-monitoring-{fixture_id}"))
            .execute(&database.pool)
            .await
            .unwrap();
        sqlx::query(
            "WITH RECURSIVE history(n) AS (
                 SELECT 0
                 UNION ALL
                 SELECT n + 1 FROM history WHERE n < 511
             )
             INSERT INTO request_stats_facts (
                 request_id, tenant_id, key_id, created_at, model, protocol,
                 status_class, error_code, upstream_account_id, model_route_id,
                 duration_ms, input_tokens, output_tokens, cached_input_tokens,
                 cache_write_tokens, service_tier, currency, cost_micros
             )
             SELECT $1 || '-request-' || n, $2, $1 || '-key', 1000 + n * 2,
                    $3, 'openai', 'success', '', $4, '', 10, 1, 1, 0, 0,
                    'default', 'USD', 1
               FROM history",
        )
        .bind(&fixture_id)
        .bind(&tenant_id)
        .bind(&model)
        .bind(&upstream_account_id)
        .execute(&database.pool)
        .await
        .unwrap();
        sqlx::query(
            "WITH RECURSIVE history(n) AS (
                 SELECT 0
                 UNION ALL
                 SELECT n + 1 FROM history WHERE n < 511
             )
             INSERT INTO generation_stats_facts (
                 job_id, tenant_id, key_id, created_at, model, status_class,
                 error_code, upstream_account_id, duration_ms, cost_micros,
                 billed_units, currency
             )
             SELECT $1 || '-generation-' || n, $2, $1 || '-key', 1001 + n * 2,
                    $3, 'success', '', $4, 10, 1, 1, 'USD'
               FROM history",
        )
        .bind(&fixture_id)
        .bind(&tenant_id)
        .bind(&model)
        .bind(&upstream_account_id)
        .execute(&database.pool)
        .await
        .unwrap();

        let mut pairs = vec![(upstream_account_id.clone(), model.clone())];
        for index in 1..10 {
            pairs.push((format!("{fixture_id}-empty-{index}"), model.clone()));
        }
        let scope = MonitoringScope::Tenant("sqlite-monitoring".to_owned());
        let mut connection = database.pool.acquire().await.unwrap();
        let mut statement_counter = MonitoringStatementCounter::default();
        let outcomes = terminal_outcomes_batch(
            &mut connection,
            MonitoringBatchContext {
                backend: DatabaseBackend::Sqlite,
                scope: &scope,
                tenant_id: &tenant_id,
                from_created_at: 1_000,
                to_created_at: 10_000,
            },
            &pairs,
            &mut statement_counter,
        )
        .await
        .unwrap();
        assert_eq!(statement_counter.count, 1);
        let selected = outcomes
            .get(&(upstream_account_id.clone(), model.clone()))
            .unwrap();
        assert_eq!(selected.len(), 5);
        assert_eq!(selected[0].id, format!("{fixture_id}-generation-511"));
        assert_eq!(selected[1].id, format!("{fixture_id}-request-511"));
        assert_eq!(selected[4].id, format!("{fixture_id}-generation-509"));
        assert!(pairs[1..].iter().all(|pair| outcomes[pair].is_empty()));

        let sql = monitoring_terminal_outcomes_batch_sql(
            &scope,
            pairs.len(),
            MonitoringTerminalBatchDialect::Sqlite,
        )
        .unwrap();
        let mut plan_query = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")));
        for (account_id, pair_model) in &pairs {
            plan_query = plan_query.bind(account_id).bind(pair_model);
        }
        let plan_rows = plan_query
            .bind(&tenant_id)
            .bind(1_000_i64)
            .bind(10_000_i64)
            .fetch_all(&mut *connection)
            .await
            .unwrap();
        let details = plan_rows
            .into_iter()
            .map(|row| row.try_get::<String, _>("detail").unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            details
                .iter()
                .filter(|detail| { detail.contains("request_stats_facts_monitoring_outcome_idx") })
                .count(),
            10
        );
        assert_eq!(
            details
                .iter()
                .filter(|detail| {
                    detail.contains("generation_stats_facts_monitoring_outcome_idx")
                })
                .count(),
            10
        );
    }

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

    #[tokio::test]
    async fn deleted_upstream_snapshot_retains_monitoring_identity_without_health_claim() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory
                .path()
                .join("deleted-upstream-monitoring.db")
                .display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let tenant_id = Uuid::now_v7().to_string();
        let upstream_account_id = Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, 1)")
            .bind(&tenant_id)
            .bind("deleted-upstream-monitoring")
            .execute(&database.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO deleted_upstream_account_snapshots (upstream_account_id, tenant_id, name, driver, auth_kind, credential_generation, created_at, deleted_at) VALUES ($1, $2, 'deleted-monitoring-provider', 'http-json', 'api_key', 3, 1, 2)",
        )
        .bind(&upstream_account_id)
        .bind(&tenant_id)
        .execute(&database.pool)
        .await
        .unwrap();
        let mut connection = database.pool.acquire().await.unwrap();
        let mut statements = MonitoringStatementCounter::default();
        let mut results = upstream_health_batch(
            &mut connection,
            std::slice::from_ref(&upstream_account_id),
            3,
            &mut statements,
        )
        .await
        .unwrap();
        let (name, health) = results.remove(&upstream_account_id).unwrap();
        assert_eq!(name, "deleted-monitoring-provider");
        assert_eq!(health.status, "unknown");
        assert_eq!(health.observed_at, None);
        assert_eq!(statements.count, 1);
    }
}
