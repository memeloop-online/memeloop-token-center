use std::collections::BTreeMap;

use sqlx::{Row, any::AnyRow};

use crate::db::usage_analysis::{approximate_p95, histogram_p95_is_capped};
use crate::db::{AppError, MAX_STATS_RANGE_MILLIS, micros_to_decimal_string};
use crate::model::{MonitoringMetrics, TokenUsage, UsageAnalysisCost};

pub(in crate::db) const HOUR_MILLIS: i64 = 3_600_000;
pub(in crate::db) const DAY_MILLIS: i64 = 86_400_000;
pub(in crate::db) const HOUR_RANGE_LIMIT: i64 = 31 * DAY_MILLIS;

#[derive(Clone, Copy, Debug)]
pub(in crate::db) enum MonitoringGranularity {
    Hour,
    Day,
}

impl MonitoringGranularity {
    pub(in crate::db) fn as_str(self) -> &'static str {
        match self {
            Self::Hour => "hour",
            Self::Day => "day",
        }
    }

    pub(in crate::db) fn rollup(self) -> (&'static str, &'static str) {
        match self {
            Self::Hour => ("usage_analysis_hourly", "hour_bucket"),
            Self::Day => ("usage_analysis_daily", "day_bucket"),
        }
    }

    pub(in crate::db) fn bucket_millis(self) -> i64 {
        match self {
            Self::Hour => HOUR_MILLIS,
            Self::Day => DAY_MILLIS,
        }
    }
}

pub(in crate::db) struct MonitoringRange {
    pub(in crate::db) from_created_at: i64,
    pub(in crate::db) to_created_at: i64,
    pub(in crate::db) granularity: MonitoringGranularity,
}

pub(in crate::db) fn validate_window(
    from_created_at: i64,
    to_created_at: i64,
    bounded: bool,
    invalid_window: &'static str,
    too_wide: &'static str,
) -> Result<MonitoringRange, AppError> {
    if from_created_at < 0 || to_created_at < 0 || from_created_at > to_created_at {
        return Err(AppError::BadRequest(invalid_window.into()));
    }
    let width = to_created_at.saturating_sub(from_created_at);
    if bounded && width > MAX_STATS_RANGE_MILLIS {
        return Err(AppError::BadRequest(too_wide.into()));
    }
    Ok(MonitoringRange {
        from_created_at,
        to_created_at,
        granularity: if width <= HOUR_RANGE_LIMIT {
            MonitoringGranularity::Hour
        } else {
            MonitoringGranularity::Day
        },
    })
}

#[derive(Clone, Copy)]
pub(in crate::db) struct BucketPlan {
    pub(in crate::db) rollup_from_bucket: i64,
    pub(in crate::db) rollup_to_bucket: i64,
    pub(in crate::db) left_from_created_at: i64,
    pub(in crate::db) left_to_created_at: i64,
    pub(in crate::db) right_from_created_at: i64,
    pub(in crate::db) right_to_created_at: i64,
}

impl BucketPlan {
    /// Preserve the usage-analysis inclusive-window contract exactly: only
    /// complete buckets hit rollups, while at most two partial edge ranges hit
    /// terminal facts.
    pub(in crate::db) fn new(from_created_at: i64, to_created_at: i64, bucket_millis: i64) -> Self {
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
pub(in crate::db) struct MetricsAccumulator {
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
    pub(in crate::db) fn accumulate(
        &mut self,
        row: &AnyRow,
        currency: String,
        invalid_currency: &'static str,
    ) -> Result<(), AppError> {
        if currency.is_empty() {
            return Err(AppError::internal(invalid_currency));
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

    pub(in crate::db) fn finish(self) -> MonitoringMetrics {
        let usage = TokenUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_input_tokens: self.cached_input_tokens,
            cache_write_tokens: self.cache_write_tokens,
            service_tier: None,
            cache_coverage: None,
        };
        MonitoringMetrics {
            requests: self.requests,
            successful_requests: self.successful_requests,
            failed_requests: self.failed_requests,
            total_tokens: usage.total_tokens(),
            cache_rate: usage.cache_rate(),
            avg_duration_ms: (self.duration_count > 0)
                .then(|| self.duration_sum_ms as f64 / self.duration_count as f64),
            p95_duration_ms: approximate_p95(self.duration_count, &self.duration_buckets),
            p95_is_capped: histogram_p95_is_capped(self.duration_count, &self.duration_buckets),
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

pub(in crate::db) fn metric_sums() -> &'static str {
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
#[derive(Clone, Copy)]
pub(in crate::db) enum MonitoringFact {
    Request,
    Generation,
}

impl MonitoringFact {
    pub(in crate::db) fn table(self) -> &'static str {
        match self {
            Self::Request => "request_stats_facts",
            Self::Generation => "generation_stats_facts",
        }
    }

    // The callers project their own grouping keys and alias the fact table as f.
    pub(in crate::db) fn metrics_projection(self) -> String {
        let tokens = match self {
            Self::Request => {
                r#"CASE
                      WHEN f.input_tokens >= f.cached_input_tokens + f.cache_write_tokens
                          THEN f.input_tokens - f.cached_input_tokens - f.cache_write_tokens
                      ELSE 0
                  END AS input_tokens,
                  f.output_tokens, f.cached_input_tokens, f.cache_write_tokens"#
            }
            Self::Generation => {
                r#"CAST(0 AS BIGINT) AS input_tokens,
                  CAST(0 AS BIGINT) AS output_tokens,
                  CAST(0 AS BIGINT) AS cached_input_tokens,
                  CAST(0 AS BIGINT) AS cache_write_tokens"#
            }
        };
        format!(
            r#"CAST(1 AS BIGINT) AS requests,
                  {tokens},
                  CAST(1 AS BIGINT) AS duration_count, f.duration_ms AS duration_sum_ms,
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
                  f.cost_micros"#,
        )
    }
}
