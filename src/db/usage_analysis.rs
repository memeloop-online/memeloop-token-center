use std::collections::BTreeMap;

use sqlx::{Row, any::AnyRow};
use uuid::Uuid;

use super::{
    AppError, Database, DatabaseBackend, MAX_STATS_RANGE_MILLIS, search_prefix, unix_millis,
};
use crate::model::{
    UsageAnalysisBucket, UsageAnalysisCost, UsageAnalysisHeatmapBucket, UsageAnalysisMetrics,
    UsageAnalysisResponse, UsageAnalysisTimeBucket, micros_to_decimal_string,
};

#[derive(Clone, Debug, Default)]
pub struct UsageAnalysisFilter {
    pub from_created_at: Option<i64>,
    pub to_created_at: Option<i64>,
    pub granularity: Option<String>,
    pub key_id: Option<Uuid>,
    pub model: Option<String>,
    pub protocol: Option<String>,
    pub status: Option<String>,
    pub error_code: Option<String>,
    pub upstream_account_id: Option<UsageAnalysisUpstreamFilter>,
    pub route_id: Option<Uuid>,
    pub key_alias: Option<String>,
    pub principal: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UsageAnalysisUpstreamFilter {
    Account(Uuid),
    Unassigned,
}

impl UsageAnalysisUpstreamFilter {
    pub fn parse(value: &str) -> Result<Self, AppError> {
        if value == "unassigned" {
            return Ok(Self::Unassigned);
        }
        Uuid::parse_str(value).map(Self::Account).map_err(|_| {
            AppError::BadRequest(
                "upstream_account_id must be a UUID or the unassigned sentinel".into(),
            )
        })
    }

    fn sql_value(&self) -> String {
        match self {
            Self::Account(id) => id.to_string(),
            Self::Unassigned => "unassigned".to_owned(),
        }
    }
}

impl Database {
    pub async fn operator_usage_analysis(
        &self,
        tenant_external_id: &str,
        filter: UsageAnalysisFilter,
    ) -> Result<UsageAnalysisResponse, AppError> {
        self.aggregate_usage_analysis(Some(tenant_external_id), &filter)
            .await
    }

    pub async fn global_usage_analysis(
        &self,
        filter: UsageAnalysisFilter,
    ) -> Result<UsageAnalysisResponse, AppError> {
        self.aggregate_usage_analysis(None, &filter).await
    }

    async fn aggregate_usage_analysis(
        &self,
        tenant_external_id: Option<&str>,
        filter: &UsageAnalysisFilter,
    ) -> Result<UsageAnalysisResponse, AppError> {
        let range = validate_usage_analysis_filter(filter)?;
        let tenant_scoped = tenant_external_id.is_some();
        let tenant_id = if let Some(external_id) = tenant_external_id {
            sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
                .bind(external_id)
                .fetch_optional(&self.pool)
                .await?
                .map(|row| row.try_get::<String, _>("id"))
                .transpose()?
                // Preserve the historical empty-result contract for an unknown
                // tenant without falling back to a global scan.
                .unwrap_or_else(|| Uuid::nil().to_string())
        } else {
            String::new()
        };
        let key_id = filter.key_id.map(|id| id.to_string()).unwrap_or_default();
        let upstream_account_id = filter
            .upstream_account_id
            .as_ref()
            .map(UsageAnalysisUpstreamFilter::sql_value)
            .unwrap_or_default();
        let route_id = filter.route_id.map(|id| id.to_string()).unwrap_or_default();
        let key_alias = search_prefix(filter.key_alias.as_deref());
        let principal = search_prefix(filter.principal.as_deref());
        let bucket_millis = match range.granularity {
            UsageAnalysisGranularity::Hour => 3_600_000,
            UsageAnalysisGranularity::Day => 86_400_000,
        };
        let main_plan =
            UsageAnalysisBucketPlan::new(range.from_created_at, range.to_created_at, bucket_millis);
        let main_sql = usage_analysis_main_sql(self.backend, range.granularity, tenant_scoped);

        macro_rules! bind_usage_filter {
            ($query:expr, $plan:expr) => {
                $query
                    .bind(&tenant_id)
                    .bind(&key_id)
                    .bind($plan.rollup_from_bucket)
                    .bind($plan.rollup_to_bucket)
                    .bind(filter.model.as_deref().unwrap_or_default())
                    .bind(filter.protocol.as_deref().unwrap_or_default())
                    .bind(filter.status.as_deref().unwrap_or_default())
                    .bind(filter.error_code.as_deref().unwrap_or_default())
                    .bind(&upstream_account_id)
                    .bind(&route_id)
                    .bind(&key_alias)
                    .bind(&principal)
                    .bind($plan.left_from_created_at)
                    .bind($plan.left_to_created_at)
                    .bind($plan.right_from_created_at)
                    .bind($plan.right_to_created_at)
            };
        }

        // SQL safety boundary: the generator accepts only backend/granularity enums and a scope
        // boolean. User filters are always bound below and are never interpolated into SQL.
        let rows = bind_usage_filter!(sqlx::query(sqlx::AssertSqlSafe(main_sql)), main_plan)
            .fetch_all(&self.pool)
            .await?;
        let mut projections: BTreeMap<(String, String), UsageMetricsAccumulator> = BTreeMap::new();
        for row in rows {
            accumulate_usage_row(&mut projections, &row)?;
        }

        let heatmap_sql = usage_analysis_heatmap_sql(tenant_scoped);
        let heatmap_plan =
            UsageAnalysisBucketPlan::new(range.from_created_at, range.to_created_at, 3_600_000);
        // Same closed generator boundary as the main analysis statement above.
        let heatmap_rows =
            bind_usage_filter!(sqlx::query(sqlx::AssertSqlSafe(heatmap_sql)), heatmap_plan)
                .fetch_all(&self.pool)
                .await?;
        let mut heatmap_projection: BTreeMap<(String, String), UsageMetricsAccumulator> =
            BTreeMap::new();
        for row in heatmap_rows {
            accumulate_usage_row(&mut heatmap_projection, &row)?;
        }

        let summary = projections
            .remove(&("summary".to_owned(), "summary".to_owned()))
            .unwrap_or_default()
            .finish();
        let mut time_series = Vec::new();
        let mut by_model = Vec::new();
        let mut by_key = Vec::new();
        let mut by_upstream = Vec::new();
        let mut by_protocol = Vec::new();
        let mut by_status = Vec::new();
        let mut errors = Vec::new();
        for ((kind, id), accumulator) in projections {
            let label = accumulator.label.clone();
            let metrics = accumulator.finish();
            match kind.as_str() {
                "time" => time_series.push(UsageAnalysisTimeBucket {
                    bucket_start: id.parse().map_err(|_| AppError::Internal)?,
                    metrics,
                }),
                "model" => by_model.push(UsageAnalysisBucket { id, label, metrics }),
                "key" => by_key.push(UsageAnalysisBucket { id, label, metrics }),
                "upstream" => by_upstream.push(UsageAnalysisBucket { id, label, metrics }),
                "protocol" => by_protocol.push(UsageAnalysisBucket { id, label, metrics }),
                "status" => {
                    let (id, label) = if id == "failure" {
                        ("error".to_owned(), "failed".to_owned())
                    } else {
                        (id, label)
                    };
                    by_status.push(UsageAnalysisBucket { id, label, metrics });
                }
                "error" => errors.push(UsageAnalysisBucket { id, label, metrics }),
                _ => return Err(AppError::Internal),
            }
        }
        time_series.sort_by_key(|bucket| bucket.bucket_start);
        for buckets in [
            &mut by_model,
            &mut by_key,
            &mut by_upstream,
            &mut by_protocol,
            &mut by_status,
            &mut errors,
        ] {
            buckets.sort_by(|left, right| {
                right
                    .metrics
                    .requests
                    .cmp(&left.metrics.requests)
                    .then_with(|| left.id.cmp(&right.id))
            });
        }
        errors.truncate(100);
        let mut heatmap = heatmap_projection
            .into_iter()
            .map(|((_kind, id), accumulator)| {
                Ok(UsageAnalysisHeatmapBucket {
                    hour_of_week: id.parse().map_err(|_| AppError::Internal)?,
                    metrics: accumulator.finish(),
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        heatmap.sort_by_key(|bucket| bucket.hour_of_week);

        Ok(UsageAnalysisResponse {
            from_created_at: range.from_created_at,
            to_created_at: range.to_created_at,
            granularity: range.granularity.as_str().to_owned(),
            time_zone: "UTC".to_owned(),
            p95_is_approximate: true,
            p95_method: "fixed_histogram_upper_bound_capped_60000ms".to_owned(),
            upstream_grouping: "stable_account".to_owned(),
            summary,
            time_series,
            by_model,
            by_key,
            by_upstream,
            by_protocol,
            by_status,
            errors,
            heatmap,
        })
    }
}

#[derive(Clone, Copy)]
enum UsageAnalysisGranularity {
    Hour,
    Day,
}

impl UsageAnalysisGranularity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hour => "hour",
            Self::Day => "day",
        }
    }
}

struct ValidatedUsageAnalysisRange {
    from_created_at: i64,
    to_created_at: i64,
    granularity: UsageAnalysisGranularity,
}

#[derive(Clone, Copy)]
struct UsageAnalysisBucketPlan {
    rollup_from_bucket: i64,
    rollup_to_bucket: i64,
    left_from_created_at: i64,
    left_to_created_at: i64,
    right_from_created_at: i64,
    right_to_created_at: i64,
}

impl UsageAnalysisBucketPlan {
    fn new(from_created_at: i64, to_created_at: i64, bucket_millis: i64) -> Self {
        let bucket_millis_i128 = i128::from(bucket_millis);
        let from_created_at_i128 = i128::from(from_created_at);
        // The public contract is inclusive at both ends.  i128 preserves the exact virtual
        // exclusive end when the inclusive end is i64::MAX, while all SQL fact bounds remain
        // representable i64 inclusive ranges.
        let to_exclusive = to_created_at
            .checked_add(1)
            .map(i128::from)
            .unwrap_or_else(|| i128::from(i64::MAX) + 1);
        let from_bucket = from_created_at_i128.div_euclid(bucket_millis_i128);
        let rollup_from_bucket = if from_created_at_i128.rem_euclid(bucket_millis_i128) == 0 {
            from_bucket
        } else {
            from_bucket.saturating_add(1)
        };
        let rollup_to_bucket = to_exclusive.div_euclid(bucket_millis_i128);
        let full_from_created_at = rollup_from_bucket.saturating_mul(bucket_millis_i128);
        let full_to_created_at = rollup_to_bucket.saturating_mul(bucket_millis_i128);
        let rollup_from_bucket = i64::try_from(rollup_from_bucket)
            .expect("non-negative i64 timestamps have an i64 bucket index");
        let rollup_to_bucket = i64::try_from(rollup_to_bucket)
            .expect("non-negative i64 timestamps have an i64 bucket index");
        if full_from_created_at < full_to_created_at {
            let (left_from_created_at, left_to_created_at) =
                inclusive_fact_bounds(from_created_at_i128, full_from_created_at.min(to_exclusive));
            let (right_from_created_at, right_to_created_at) =
                inclusive_fact_bounds(full_to_created_at.max(from_created_at_i128), to_exclusive);
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

fn inclusive_fact_bounds(from_created_at: i128, to_exclusive: i128) -> (i64, i64) {
    if from_created_at >= to_exclusive {
        return (0, -1);
    }
    (
        i64::try_from(from_created_at).expect("fact range start is an i64 timestamp"),
        i64::try_from(to_exclusive - 1).expect("fact range end is an i64 timestamp"),
    )
}

fn validate_usage_analysis_filter(
    filter: &UsageAnalysisFilter,
) -> Result<ValidatedUsageAnalysisRange, AppError> {
    const HOUR_RANGE_LIMIT: i64 = 31 * 86_400_000;
    let requested = filter.granularity.as_deref().unwrap_or("auto");
    if !matches!(requested, "auto" | "hour" | "day") {
        return Err(AppError::BadRequest(
            "granularity must be auto, hour, or day".into(),
        ));
    }
    let to = filter.to_created_at.unwrap_or_else(unix_millis);
    let default_window = if requested == "day" {
        7 * 86_400_000
    } else {
        24 * 60 * 60 * 1_000
    };
    let from = filter
        .from_created_at
        .unwrap_or_else(|| to.saturating_sub(default_window));
    if from < 0 || to < 0 || from > to {
        return Err(AppError::BadRequest(
            "usage analysis requires a valid non-negative from_created_at/to_created_at range"
                .into(),
        ));
    }
    let range = to.saturating_sub(from);
    if range > MAX_STATS_RANGE_MILLIS {
        return Err(AppError::BadRequest(
            "usage analysis range must not exceed 93 days".into(),
        ));
    }
    let granularity = match requested {
        "hour" if range > HOUR_RANGE_LIMIT => {
            return Err(AppError::BadRequest(
                "hour granularity range must not exceed 31 days".into(),
            ));
        }
        "hour" => UsageAnalysisGranularity::Hour,
        "day" => UsageAnalysisGranularity::Day,
        "auto" if range <= HOUR_RANGE_LIMIT => UsageAnalysisGranularity::Hour,
        "auto" => UsageAnalysisGranularity::Day,
        _ => unreachable!(),
    };
    if filter
        .status
        .as_deref()
        .is_some_and(|value| !matches!(value, "success" | "error"))
    {
        return Err(AppError::BadRequest(
            "status must be success or error for usage analysis".into(),
        ));
    }
    if filter.protocol.as_deref().is_some_and(|value| {
        !matches!(
            value,
            "openai" | "anthropic" | "openai-image" | "generation"
        )
    }) {
        return Err(AppError::BadRequest(
            "protocol must be openai, anthropic, openai-image, or generation".into(),
        ));
    }
    for (name, value) in [
        ("model", filter.model.as_deref()),
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
    Ok(ValidatedUsageAnalysisRange {
        from_created_at: from,
        to_created_at: to,
        granularity,
    })
}

const USAGE_ANALYSIS_METRIC_SUMS: &str = r#"
CAST(COALESCE(SUM(requests), 0) AS BIGINT) AS requests,
CAST(COALESCE(SUM(CASE WHEN status_class = 'success' THEN requests ELSE 0 END), 0) AS BIGINT) AS successful_requests,
CAST(COALESCE(SUM(CASE WHEN status_class = 'failure' THEN requests ELSE 0 END), 0) AS BIGINT) AS failed_requests,
CAST(COALESCE(SUM(input_tokens), 0) AS BIGINT) AS input_tokens,
CAST(COALESCE(SUM(output_tokens), 0) AS BIGINT) AS output_tokens,
CAST(COALESCE(SUM(cached_input_tokens), 0) AS BIGINT) AS cached_input_tokens,
CAST(COALESCE(SUM(cache_write_tokens), 0) AS BIGINT) AS cache_write_tokens,
CAST(COALESCE(SUM(generation_units), 0) AS BIGINT) AS generation_units,
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
CAST(COALESCE(SUM(cost_micros), 0) AS BIGINT) AS cost_micros
"#;

fn usage_analysis_source_sql(granularity: UsageAnalysisGranularity, tenant_scoped: bool) -> String {
    let (table, bucket_column, bucket_millis) = match granularity {
        UsageAnalysisGranularity::Hour => ("usage_analysis_hourly", "hour_bucket", 3_600_000),
        UsageAnalysisGranularity::Day => ("usage_analysis_daily", "day_bucket", 86_400_000),
    };
    let tenant_predicate = if tenant_scoped {
        "AND {alias}.tenant_id = $1"
    } else {
        // Keep the stable bind layout without an optional tenant OR predicate.
        "AND CAST($1 AS TEXT) = ''"
    };
    let branch_filters = |alias: &str| {
        format!(
            r#"{tenant_predicate}
              AND ($2 = '' OR {alias}.key_id = $2)
              AND ($5 = '' OR {alias}.model = $5)
              AND ($8 = '' OR {alias}.error_code = $8)
              AND ($9 = ''
                   OR ($9 = 'unassigned' AND {alias}.upstream_account_id = '')
                   OR {alias}.upstream_account_id = $9)
              AND ($10 = '' OR {alias}.model_route_id = $10)"#,
            tenant_predicate = tenant_predicate.replace("{alias}", alias),
        )
    };
    let rollup_filters = branch_filters("a");
    let rollup = format!(
        r#"SELECT a.tenant_id, a.key_id,
                  a.{bucket_column} * {bucket_millis} AS bucket_start,
                  a.model, a.protocol, a.status_class, a.error_code,
                  a.upstream_account_id, a.model_route_id, a.currency,
                  a.requests, a.input_tokens, a.output_tokens,
                  a.cached_input_tokens, a.cache_write_tokens, a.generation_units,
                  a.duration_count, a.duration_sum_ms,
                  a.duration_bucket_0, a.duration_bucket_1, a.duration_bucket_2,
                  a.duration_bucket_3, a.duration_bucket_4, a.duration_bucket_5,
                  a.duration_bucket_6, a.duration_bucket_7, a.duration_bucket_8,
                  a.duration_bucket_9, a.duration_bucket_10, a.duration_bucket_11,
                  a.cost_micros
             FROM {table} a
            WHERE a.{bucket_column} >= $3 AND a.{bucket_column} < $4
              {rollup_filters}"#
    );
    let request_filters = branch_filters("f");
    let generation_filters = request_filters.replace("f.model_route_id", "''");
    let left_requests =
        usage_analysis_request_fact_sql("$13", "$14", bucket_millis, &request_filters);
    let left_generations =
        usage_analysis_generation_fact_sql("$13", "$14", bucket_millis, &generation_filters);
    let right_requests =
        usage_analysis_request_fact_sql("$15", "$16", bucket_millis, &request_filters);
    let right_generations =
        usage_analysis_generation_fact_sql("$15", "$16", bucket_millis, &generation_filters);
    format!(
        r#"SELECT activity.*,
                  k.alias AS key_label,
                  CASE WHEN activity.upstream_account_id = '' THEN 'unassigned'
                       ELSE activity.upstream_account_id END AS analysis_upstream_id,
                  CASE WHEN activity.upstream_account_id = '' THEN 'Unassigned'
                       ELSE COALESCE(u.name, activity.upstream_account_id) END AS upstream_label
             FROM (
                  {rollup}
                  UNION ALL
                  {left_requests}
                  UNION ALL
                  {left_generations}
                  UNION ALL
                  {right_requests}
                  UNION ALL
                  {right_generations}
             ) activity
             JOIN key_records k
               ON k.id = activity.key_id AND k.tenant_id = activity.tenant_id
             JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id
             LEFT JOIN upstream_accounts u
                    ON u.id = activity.upstream_account_id
                   AND u.tenant_id = activity.tenant_id
            WHERE ($2 = '' OR activity.key_id = $2)
              AND ($5 = '' OR activity.model = $5)
              AND ($6 = '' OR activity.protocol = $6)
              AND ($7 = ''
                   OR ($7 = 'success' AND activity.status_class = 'success')
                   OR ($7 = 'error' AND activity.status_class = 'failure'))
              AND ($8 = '' OR activity.error_code = $8)
              AND ($9 = ''
                   OR ($9 = 'unassigned' AND activity.upstream_account_id = '')
                   OR activity.upstream_account_id = $9)
              AND ($10 = '' OR activity.model_route_id = $10)
              AND ($11 = '' OR LOWER(k.alias) LIKE $11 ESCAPE '\')
              AND ($12 = '' OR LOWER(p.external_id) LIKE $12 ESCAPE '\')"#
    )
}

fn usage_analysis_request_fact_sql(
    from_parameter: &str,
    to_parameter: &str,
    bucket_millis: i64,
    branch_filters: &str,
) -> String {
    format!(
        r#"SELECT f.tenant_id, f.key_id,
                  (f.created_at / {bucket_millis}) * {bucket_millis} AS bucket_start,
                  f.model,
                  CASE
                      WHEN f.protocol = 'anthropic' OR f.protocol LIKE 'anthropic-%'
                          THEN 'anthropic'
                      WHEN f.protocol = 'openai-image' THEN 'openai-image'
                      ELSE 'openai'
                  END AS protocol,
                  f.status_class, f.error_code, f.upstream_account_id,
                  f.model_route_id, f.currency,
                  CAST(1 AS BIGINT) AS requests,
                  CASE
                      WHEN f.input_tokens >= f.cached_input_tokens + f.cache_write_tokens
                          THEN f.input_tokens - f.cached_input_tokens - f.cache_write_tokens
                      ELSE 0
                  END AS input_tokens,
                  f.output_tokens, f.cached_input_tokens, f.cache_write_tokens,
                  CAST(0 AS BIGINT) AS generation_units,
                  CAST(1 AS BIGINT) AS duration_count,
                  f.duration_ms AS duration_sum_ms,
                  CASE WHEN f.duration_ms <= 10 THEN 1 ELSE 0 END AS duration_bucket_0,
                  CASE WHEN f.duration_ms > 10 AND f.duration_ms <= 50 THEN 1 ELSE 0 END AS duration_bucket_1,
                  CASE WHEN f.duration_ms > 50 AND f.duration_ms <= 100 THEN 1 ELSE 0 END AS duration_bucket_2,
                  CASE WHEN f.duration_ms > 100 AND f.duration_ms <= 250 THEN 1 ELSE 0 END AS duration_bucket_3,
                  CASE WHEN f.duration_ms > 250 AND f.duration_ms <= 500 THEN 1 ELSE 0 END AS duration_bucket_4,
                  CASE WHEN f.duration_ms > 500 AND f.duration_ms <= 1000 THEN 1 ELSE 0 END AS duration_bucket_5,
                  CASE WHEN f.duration_ms > 1000 AND f.duration_ms <= 2500 THEN 1 ELSE 0 END AS duration_bucket_6,
                  CASE WHEN f.duration_ms > 2500 AND f.duration_ms <= 5000 THEN 1 ELSE 0 END AS duration_bucket_7,
                  CASE WHEN f.duration_ms > 5000 AND f.duration_ms <= 10000 THEN 1 ELSE 0 END AS duration_bucket_8,
                  CASE WHEN f.duration_ms > 10000 AND f.duration_ms <= 30000 THEN 1 ELSE 0 END AS duration_bucket_9,
                  CASE WHEN f.duration_ms > 30000 AND f.duration_ms <= 60000 THEN 1 ELSE 0 END AS duration_bucket_10,
                  CASE WHEN f.duration_ms > 60000 THEN 1 ELSE 0 END AS duration_bucket_11,
                  f.cost_micros
            FROM request_stats_facts f
            WHERE {from_parameter} <= {to_parameter}
              AND f.created_at >= {from_parameter}
              AND f.created_at <= {to_parameter}
              {branch_filters}"#
    )
}

fn usage_analysis_generation_fact_sql(
    from_parameter: &str,
    to_parameter: &str,
    bucket_millis: i64,
    branch_filters: &str,
) -> String {
    format!(
        r#"SELECT f.tenant_id, f.key_id,
                  (f.created_at / {bucket_millis}) * {bucket_millis} AS bucket_start,
                  f.model, 'generation' AS protocol, f.status_class, f.error_code,
                  f.upstream_account_id, '' AS model_route_id, f.currency,
                  CAST(1 AS BIGINT) AS requests,
                  CAST(0 AS BIGINT) AS input_tokens,
                  CAST(0 AS BIGINT) AS output_tokens,
                  CAST(0 AS BIGINT) AS cached_input_tokens,
                  CAST(0 AS BIGINT) AS cache_write_tokens,
                  f.billed_units AS generation_units,
                  CAST(1 AS BIGINT) AS duration_count,
                  f.duration_ms AS duration_sum_ms,
                  CASE WHEN f.duration_ms <= 10 THEN 1 ELSE 0 END AS duration_bucket_0,
                  CASE WHEN f.duration_ms > 10 AND f.duration_ms <= 50 THEN 1 ELSE 0 END AS duration_bucket_1,
                  CASE WHEN f.duration_ms > 50 AND f.duration_ms <= 100 THEN 1 ELSE 0 END AS duration_bucket_2,
                  CASE WHEN f.duration_ms > 100 AND f.duration_ms <= 250 THEN 1 ELSE 0 END AS duration_bucket_3,
                  CASE WHEN f.duration_ms > 250 AND f.duration_ms <= 500 THEN 1 ELSE 0 END AS duration_bucket_4,
                  CASE WHEN f.duration_ms > 500 AND f.duration_ms <= 1000 THEN 1 ELSE 0 END AS duration_bucket_5,
                  CASE WHEN f.duration_ms > 1000 AND f.duration_ms <= 2500 THEN 1 ELSE 0 END AS duration_bucket_6,
                  CASE WHEN f.duration_ms > 2500 AND f.duration_ms <= 5000 THEN 1 ELSE 0 END AS duration_bucket_7,
                  CASE WHEN f.duration_ms > 5000 AND f.duration_ms <= 10000 THEN 1 ELSE 0 END AS duration_bucket_8,
                  CASE WHEN f.duration_ms > 10000 AND f.duration_ms <= 30000 THEN 1 ELSE 0 END AS duration_bucket_9,
                  CASE WHEN f.duration_ms > 30000 AND f.duration_ms <= 60000 THEN 1 ELSE 0 END AS duration_bucket_10,
                  CASE WHEN f.duration_ms > 60000 THEN 1 ELSE 0 END AS duration_bucket_11,
                  f.cost_micros
             FROM generation_stats_facts f
            WHERE {from_parameter} <= {to_parameter}
              AND f.created_at >= {from_parameter}
              AND f.created_at <= {to_parameter}
              {branch_filters}"#
    )
}

fn usage_analysis_main_sql(
    backend: DatabaseBackend,
    granularity: UsageAnalysisGranularity,
    tenant_scoped: bool,
) -> String {
    let source = usage_analysis_source_sql(granularity, tenant_scoped);
    let grouped = match backend {
        DatabaseBackend::PostgreSql => format!(
            r#"SELECT CASE
                         WHEN GROUPING(bucket_start) = 0 THEN 'time'
                         WHEN GROUPING(model) = 0 THEN 'model'
                         WHEN GROUPING(key_id) = 0 THEN 'key'
                         WHEN GROUPING(analysis_upstream_id) = 0 THEN 'upstream'
                         WHEN GROUPING(protocol) = 0 THEN 'protocol'
                         WHEN GROUPING(status_class) = 0 THEN 'status'
                         WHEN GROUPING(error_code) = 0 THEN 'error'
                         ELSE 'summary' END AS bucket_kind,
                     CASE
                         WHEN GROUPING(bucket_start) = 0 THEN CAST(bucket_start AS TEXT)
                         WHEN GROUPING(model) = 0 THEN model
                         WHEN GROUPING(key_id) = 0 THEN key_id
                         WHEN GROUPING(analysis_upstream_id) = 0 THEN analysis_upstream_id
                         WHEN GROUPING(protocol) = 0 THEN protocol
                         WHEN GROUPING(status_class) = 0 THEN status_class
                         WHEN GROUPING(error_code) = 0 THEN error_code
                         ELSE 'summary' END AS bucket_id,
                     CASE
                         WHEN GROUPING(bucket_start) = 0 THEN CAST(bucket_start AS TEXT)
                         WHEN GROUPING(model) = 0 THEN model
                         WHEN GROUPING(key_id) = 0 THEN key_label
                         WHEN GROUPING(analysis_upstream_id) = 0 THEN upstream_label
                         WHEN GROUPING(protocol) = 0 THEN protocol
                         WHEN GROUPING(status_class) = 0 THEN status_class
                         WHEN GROUPING(error_code) = 0 THEN error_code
                         ELSE 'summary' END AS bucket_label,
                     currency,
                     {sums}
                FROM filtered_activity
               GROUP BY GROUPING SETS (
                   (currency), (bucket_start, currency), (model, currency),
                   (key_id, key_label, currency),
                   (analysis_upstream_id, upstream_label, currency),
                   (protocol, currency), (status_class, currency), (error_code, currency)
               )
              HAVING GROUPING(error_code) = 1 OR error_code <> ''"#,
            sums = USAGE_ANALYSIS_METRIC_SUMS
        ),
        DatabaseBackend::Sqlite => {
            let projections = [
                ("summary", "'summary'", "'summary'", "currency", ""),
                (
                    "time",
                    "CAST(bucket_start AS TEXT)",
                    "CAST(bucket_start AS TEXT)",
                    "bucket_start, currency",
                    "",
                ),
                ("model", "model", "model", "model, currency", ""),
                (
                    "key",
                    "key_id",
                    "key_label",
                    "key_id, key_label, currency",
                    "",
                ),
                (
                    "upstream",
                    "analysis_upstream_id",
                    "upstream_label",
                    "analysis_upstream_id, upstream_label, currency",
                    "",
                ),
                ("protocol", "protocol", "protocol", "protocol, currency", ""),
                (
                    "status",
                    "status_class",
                    "status_class",
                    "status_class, currency",
                    "",
                ),
                (
                    "error",
                    "error_code",
                    "error_code",
                    "error_code, currency",
                    "WHERE error_code <> ''",
                ),
            ];
            projections
                .into_iter()
                .map(|(kind, id, label, groups, condition)| {
                    format!(
                        "SELECT '{kind}' AS bucket_kind, {id} AS bucket_id, {label} AS bucket_label, currency, {sums} FROM filtered_activity {condition} GROUP BY {groups}",
                        sums = USAGE_ANALYSIS_METRIC_SUMS
                    )
                })
                .collect::<Vec<_>>()
                .join(" UNION ALL ")
        }
    };
    format!(
        r#"WITH filtered_activity AS MATERIALIZED ({source}),
grouped AS ({grouped}),
with_bucket_totals AS (
    SELECT grouped.*,
           SUM(requests) OVER (PARTITION BY bucket_kind, bucket_id) AS bucket_requests
      FROM grouped
),
ranked AS (
    SELECT with_bucket_totals.*,
           DENSE_RANK() OVER (
               PARTITION BY bucket_kind ORDER BY bucket_requests DESC, bucket_id ASC
           ) AS bucket_rank
      FROM with_bucket_totals
)
SELECT bucket_kind, bucket_id, bucket_label, currency, requests, successful_requests,
       failed_requests, input_tokens, output_tokens, cached_input_tokens,
       cache_write_tokens, generation_units, duration_count, duration_sum_ms,
       duration_bucket_0, duration_bucket_1, duration_bucket_2, duration_bucket_3,
       duration_bucket_4, duration_bucket_5, duration_bucket_6, duration_bucket_7,
       duration_bucket_8, duration_bucket_9, duration_bucket_10, duration_bucket_11,
       cost_micros
  FROM ranked
 WHERE bucket_kind IN ('summary', 'time') OR bucket_rank <= 100
 ORDER BY CASE bucket_kind WHEN 'summary' THEN 0 WHEN 'time' THEN 1 ELSE 2 END,
          bucket_kind, bucket_id, currency"#
    )
}

fn usage_analysis_heatmap_sql(tenant_scoped: bool) -> String {
    let source = usage_analysis_source_sql(UsageAnalysisGranularity::Hour, tenant_scoped);
    format!(
        r#"WITH filtered_activity AS MATERIALIZED ({source})
SELECT 'heatmap' AS bucket_kind,
       CAST((bucket_start / 3600000 + 72) % 168 AS TEXT) AS bucket_id,
       CAST((bucket_start / 3600000 + 72) % 168 AS TEXT) AS bucket_label,
       currency,
       {sums}
  FROM filtered_activity
 GROUP BY (bucket_start / 3600000 + 72) % 168, currency
 ORDER BY (bucket_start / 3600000 + 72) % 168, currency"#,
        sums = USAGE_ANALYSIS_METRIC_SUMS
    )
}

#[derive(Default)]
struct UsageMetricsAccumulator {
    label: String,
    requests: i64,
    successful_requests: i64,
    failed_requests: i64,
    input_tokens: i64,
    output_tokens: i64,
    cached_input_tokens: i64,
    cache_write_tokens: i64,
    generation_units: i64,
    duration_count: i64,
    duration_sum_ms: i64,
    duration_buckets: [i64; 12],
    costs: BTreeMap<String, i64>,
}

impl UsageMetricsAccumulator {
    fn finish(self) -> UsageAnalysisMetrics {
        let avg_duration_ms = (self.duration_count > 0)
            .then(|| self.duration_sum_ms as f64 / self.duration_count as f64);
        let p95_duration_ms = approximate_p95(self.duration_count, &self.duration_buckets);
        UsageAnalysisMetrics {
            requests: self.requests,
            success: self.successful_requests,
            failed: self.failed_requests,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_input_tokens: self.cached_input_tokens,
            cache_write_tokens: self.cache_write_tokens,
            generation_units: self.generation_units,
            avg_duration_ms,
            p95_duration_ms,
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

fn accumulate_usage_row(
    projections: &mut BTreeMap<(String, String), UsageMetricsAccumulator>,
    row: &AnyRow,
) -> Result<(), AppError> {
    let kind: String = row.try_get("bucket_kind")?;
    let id: String = row.try_get("bucket_id")?;
    let label: String = row.try_get("bucket_label")?;
    let accumulator = projections.entry((kind, id)).or_default();
    if accumulator.label.is_empty() {
        accumulator.label = label;
    }
    accumulator.requests = accumulator
        .requests
        .saturating_add(row.try_get("requests")?);
    accumulator.successful_requests = accumulator
        .successful_requests
        .saturating_add(row.try_get("successful_requests")?);
    accumulator.failed_requests = accumulator
        .failed_requests
        .saturating_add(row.try_get("failed_requests")?);
    accumulator.input_tokens = accumulator
        .input_tokens
        .saturating_add(row.try_get("input_tokens")?);
    accumulator.output_tokens = accumulator
        .output_tokens
        .saturating_add(row.try_get("output_tokens")?);
    accumulator.cached_input_tokens = accumulator
        .cached_input_tokens
        .saturating_add(row.try_get("cached_input_tokens")?);
    accumulator.cache_write_tokens = accumulator
        .cache_write_tokens
        .saturating_add(row.try_get("cache_write_tokens")?);
    accumulator.generation_units = accumulator
        .generation_units
        .saturating_add(row.try_get("generation_units")?);
    accumulator.duration_count = accumulator
        .duration_count
        .saturating_add(row.try_get("duration_count")?);
    accumulator.duration_sum_ms = accumulator
        .duration_sum_ms
        .saturating_add(row.try_get("duration_sum_ms")?);
    for (index, bucket) in accumulator.duration_buckets.iter_mut().enumerate() {
        let column = format!("duration_bucket_{index}");
        *bucket = bucket.saturating_add(row.try_get(column.as_str())?);
    }
    let currency: String = row.try_get("currency")?;
    let cost_micros: i64 = row.try_get("cost_micros")?;
    if currency.is_empty() {
        return Err(AppError::Internal);
    }
    let cost = accumulator.costs.entry(currency).or_default();
    *cost = cost.saturating_add(cost_micros);
    Ok(())
}

fn approximate_p95(duration_count: i64, buckets: &[i64; 12]) -> Option<i64> {
    if duration_count <= 0 {
        return None;
    }
    let target = duration_count.saturating_mul(95).saturating_add(99) / 100;
    let upper_bounds = [
        10, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000, 60_000,
    ];
    let mut cumulative = 0_i64;
    for (count, upper_bound) in buckets.iter().zip(upper_bounds) {
        cumulative = cumulative.saturating_add(*count);
        if cumulative >= target {
            return Some(upper_bound);
        }
    }
    Some(60_000)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use sqlx::Row;

    use super::{
        Database, DatabaseBackend, UsageAnalysisBucketPlan, UsageAnalysisGranularity,
        UsageMetricsAccumulator, usage_analysis_heatmap_sql, usage_analysis_main_sql,
    };

    #[test]
    fn inclusive_bucket_plan_separates_exact_edges_from_complete_interior() {
        let plan = UsageAnalysisBucketPlan::new(900, 9_900, 3_600);
        assert_eq!(plan.rollup_from_bucket, 1);
        assert_eq!(plan.rollup_to_bucket, 2);
        assert_eq!(
            (plan.left_from_created_at, plan.left_to_created_at),
            (900, 3_599)
        );
        assert_eq!(
            (plan.right_from_created_at, plan.right_to_created_at),
            (7_200, 9_900)
        );
    }

    #[test]
    fn inclusive_bucket_plan_handles_aligned_and_maximum_timestamps() {
        let aligned = UsageAnalysisBucketPlan::new(3_600, 7_199, 3_600);
        assert_eq!(aligned.rollup_from_bucket, 1);
        assert_eq!(aligned.rollup_to_bucket, 2);
        assert!(aligned.left_from_created_at > aligned.left_to_created_at);
        assert!(aligned.right_from_created_at > aligned.right_to_created_at);

        let maximum = UsageAnalysisBucketPlan::new(i64::MAX, i64::MAX, 3_600_000);
        assert_eq!(maximum.left_from_created_at, i64::MAX);
        assert_eq!(maximum.left_to_created_at, i64::MAX);
        assert!(maximum.right_from_created_at > maximum.right_to_created_at);
    }

    #[test]
    fn maximum_daily_range_uses_only_complete_rollup_buckets() {
        const DAY: i64 = 86_400_000;
        let from = 20_000 * DAY;
        let to = from + 93 * DAY - 1;
        let plan = UsageAnalysisBucketPlan::new(from, to, DAY);
        assert_eq!(plan.rollup_from_bucket, 20_000);
        assert_eq!(plan.rollup_to_bucket, 20_093);
        assert!(plan.left_from_created_at > plan.left_to_created_at);
        assert!(plan.right_from_created_at > plan.right_to_created_at);
    }

    #[test]
    fn costs_remain_separate_and_deterministically_sorted_by_currency() {
        let metrics = UsageMetricsAccumulator {
            costs: BTreeMap::from([("USD".to_owned(), 1_250_000), ("CNY".to_owned(), 2_500_000)]),
            ..UsageMetricsAccumulator::default()
        }
        .finish();
        assert_eq!(metrics.costs.len(), 2);
        assert_eq!(metrics.costs[0].currency, "CNY");
        assert_eq!(metrics.costs[0].cost, "2.5");
        assert_eq!(metrics.costs[1].currency, "USD");
        assert_eq!(metrics.costs[1].cost, "1.25");
    }

    #[test]
    fn aggregate_sql_groups_every_projection_by_currency() {
        for backend in [DatabaseBackend::PostgreSql, DatabaseBackend::Sqlite] {
            let sql = usage_analysis_main_sql(backend, UsageAnalysisGranularity::Day, false);
            assert!(sql.contains("currency"));
            assert!(!sql.contains("request_records"));
            assert!(!sql.contains("generation_jobs"));
            match backend {
                DatabaseBackend::PostgreSql => {
                    assert!(sql.contains("(bucket_start, currency)"));
                    assert!(sql.contains("(analysis_upstream_id, upstream_label, currency)"));
                }
                DatabaseBackend::Sqlite => {
                    assert!(sql.contains("GROUP BY bucket_start, currency"));
                    assert!(
                        sql.contains("GROUP BY analysis_upstream_id, upstream_label, currency")
                    );
                }
            }
        }
    }

    #[test]
    fn tenant_scope_is_pushed_into_every_rollup_and_fact_branch() {
        let sql = usage_analysis_main_sql(
            DatabaseBackend::PostgreSql,
            UsageAnalysisGranularity::Day,
            true,
        );
        assert!(sql.contains("a.tenant_id = $1"), "{sql}");
        assert_eq!(sql.matches("f.tenant_id = $1").count(), 4, "{sql}");
        assert!(!sql.contains("JOIN tenants"), "{sql}");
        assert!(!sql.contains("$1 = '' OR"), "{sql}");
    }

    #[tokio::test]
    async fn postgres_boundary_queries_use_bounded_fact_index_ranges() {
        let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        // Unit tests run before integration-test binaries in the same Cargo
        // invocation, so this plan check must not depend on another test
        // having migrated the shared CI PostgreSQL service first.
        let database = Database::connect_with_max(&database_url, 1).await.unwrap();
        database.migrate().await.unwrap();
        let pool = &database.pool;
        sqlx::query("SET enable_seqscan = off")
            .execute(pool)
            .await
            .unwrap();

        for (sql, plan, rollup_index_prefix, bucket_column) in [
            {
                let bucket_millis = 3_600_000;
                let bucket_start = 1_786_982_400_000_i64;
                let from = bucket_start - 15 * 60_000;
                let to = bucket_start + bucket_millis + 15 * 60_000;
                (
                    usage_analysis_main_sql(
                        DatabaseBackend::PostgreSql,
                        UsageAnalysisGranularity::Hour,
                        true,
                    ),
                    UsageAnalysisBucketPlan::new(from, to, bucket_millis),
                    "Index Scan using usage_analysis_hourly_",
                    "hour_bucket",
                )
            },
            {
                let bucket_millis = 86_400_000;
                let bucket_start = 1_786_924_800_000_i64;
                let from = bucket_start - 15 * 60_000;
                let to = bucket_start + bucket_millis + 15 * 60_000;
                (
                    usage_analysis_main_sql(
                        DatabaseBackend::PostgreSql,
                        UsageAnalysisGranularity::Day,
                        true,
                    ),
                    UsageAnalysisBucketPlan::new(from, to, bucket_millis),
                    "Index Scan using usage_analysis_daily_",
                    "day_bucket",
                )
            },
            {
                let bucket_millis = 86_400_000;
                let from = 20_000 * bucket_millis;
                let to = from + 93 * bucket_millis - 1;
                (
                    usage_analysis_main_sql(
                        DatabaseBackend::PostgreSql,
                        UsageAnalysisGranularity::Day,
                        true,
                    ),
                    UsageAnalysisBucketPlan::new(from, to, bucket_millis),
                    "Index Scan using usage_analysis_daily_",
                    "day_bucket",
                )
            },
        ] {
            let expected_fact_scans =
                usize::from(plan.left_from_created_at <= plan.left_to_created_at)
                    + usize::from(plan.right_from_created_at <= plan.right_to_created_at);
            let explain = explain_usage_query(pool, &sql, plan).await;
            // PostgreSQL can legitimately select either the plain tenant/time
            // index or a more selective tenant/dimension/time index (including
            // skip scans) as statistics change. Assert the bounded access
            // semantics rather than freezing one planner-selected index name.
            assert!(explain.contains(rollup_index_prefix), "{explain}");
            assert!(explain.contains("tenant_id ="), "{explain}");
            assert!(
                explain.contains(&format!("{bucket_column} >=")),
                "{explain}"
            );
            assert!(
                explain
                    .matches("Index Scan using request_stats_facts_")
                    .count()
                    >= expected_fact_scans,
                "{explain}"
            );
            assert!(
                explain
                    .matches("Index Scan using generation_stats_facts_")
                    .count()
                    >= expected_fact_scans,
                "{explain}"
            );
            assert!(
                !explain.contains("Seq Scan on request_stats_facts"),
                "{explain}"
            );
            assert!(
                !explain.contains("Seq Scan on generation_stats_facts"),
                "{explain}"
            );
            assert!(!explain.contains("request_records"), "{explain}");
            assert!(!explain.contains("generation_jobs"), "{explain}");
        }

        let bucket_millis = 3_600_000;
        let bucket_start = 1_786_982_400_000_i64;
        let plan = UsageAnalysisBucketPlan::new(
            bucket_start - 15 * 60_000,
            bucket_start + bucket_millis + 15 * 60_000,
            bucket_millis,
        );
        let heatmap = explain_usage_query(pool, &usage_analysis_heatmap_sql(true), plan).await;
        assert!(
            heatmap.contains("Index Scan using usage_analysis_hourly_"),
            "{heatmap}"
        );
        assert!(heatmap.contains("tenant_id ="), "{heatmap}");
        assert!(heatmap.contains("hour_bucket >="), "{heatmap}");
        assert!(
            heatmap
                .matches("Index Scan using request_stats_facts_")
                .count()
                >= 2,
            "{heatmap}"
        );
        assert!(
            heatmap
                .matches("Index Scan using generation_stats_facts_")
                .count()
                >= 2,
            "{heatmap}"
        );
        assert!(
            !heatmap.contains("Seq Scan on request_stats_facts"),
            "{heatmap}"
        );
        assert!(
            !heatmap.contains("Seq Scan on generation_stats_facts"),
            "{heatmap}"
        );
        database.pool.close().await;
    }

    async fn explain_usage_query(
        pool: &sqlx::AnyPool,
        sql: &str,
        plan: UsageAnalysisBucketPlan,
    ) -> String {
        let explain_sql =
            format!("EXPLAIN (ANALYZE, BUFFERS, TIMING OFF, COSTS OFF, FORMAT TEXT) {sql}");
        // Test-only safety boundary: every caller passes output from the closed usage-analysis
        // generators above. This helper is not exposed to request or configuration input.
        let rows = sqlx::query(sqlx::AssertSqlSafe(explain_sql))
            .bind("00000000-0000-0000-0000-000000000001")
            .bind("")
            .bind(plan.rollup_from_bucket)
            .bind(plan.rollup_to_bucket)
            .bind("")
            .bind("")
            .bind("")
            .bind("")
            .bind("")
            .bind("")
            .bind("")
            .bind("")
            .bind(plan.left_from_created_at)
            .bind(plan.left_to_created_at)
            .bind(plan.right_from_created_at)
            .bind(plan.right_to_created_at)
            .fetch_all(pool)
            .await
            .unwrap();
        rows.into_iter()
            .map(|row| row.try_get::<String, _>(0).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }
}
