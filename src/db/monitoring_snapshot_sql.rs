use super::{MonitoringGranularity, MonitoringScope};

pub(super) const TOP_UPSTREAM_MODEL_LIMIT: usize = 10;
pub(super) const TERMINAL_OUTCOME_LIMIT: usize = 5;

/// The compact aggregate query behind the operator monitoring snapshot.
///
/// Complete buckets use the immutable usage rollups. The two partial buckets
/// use terminal facts only, so the exact inclusive time window never turns
/// into an unbounded `request_records` scan.
pub(super) fn monitoring_snapshot_sql(
    scope: &MonitoringScope,
    granularity: MonitoringGranularity,
) -> String {
    let (table, bucket_column, _bucket_millis) = match granularity {
        MonitoringGranularity::Hour => ("usage_analysis_hourly", "hour_bucket", 3_600_000),
        MonitoringGranularity::Day => ("usage_analysis_daily", "day_bucket", 86_400_000),
    };
    let (rollup_predicate, fact_predicate, left_from, left_to, right_from, right_to) = match scope {
        MonitoringScope::Tenant(_) => (
            format!("a.tenant_id = $1 AND a.{bucket_column} >= $2 AND a.{bucket_column} < $3"),
            "f.tenant_id = $1",
            "$4",
            "$5",
            "$6",
            "$7",
        ),
        MonitoringScope::Global => (
            format!("a.{bucket_column} >= $1 AND a.{bucket_column} < $2"),
            "1 = 1",
            "$3",
            "$4",
            "$5",
            "$6",
        ),
    };
    let rollup = format!(
        r#"SELECT a.upstream_account_id, a.model, a.status_class, a.currency,
                  a.requests, a.duration_count, a.duration_sum_ms,
                  a.duration_bucket_0, a.duration_bucket_1, a.duration_bucket_2,
                  a.duration_bucket_3, a.duration_bucket_4, a.duration_bucket_5,
                  a.duration_bucket_6, a.duration_bucket_7, a.duration_bucket_8,
                  a.duration_bucket_9, a.duration_bucket_10, a.duration_bucket_11,
                  a.cost_micros
             FROM {table} a
            WHERE {rollup_predicate}"#,
    );
    let left_requests = request_fact_sql(left_from, left_to, fact_predicate);
    let left_generations = generation_fact_sql(left_from, left_to, fact_predicate);
    let right_requests = request_fact_sql(right_from, right_to, fact_predicate);
    let right_generations = generation_fact_sql(right_from, right_to, fact_predicate);
    format!(
        r#"WITH source AS MATERIALIZED (
    {rollup}
    UNION ALL
    {left_requests}
    UNION ALL
    {left_generations}
    UNION ALL
    {right_requests}
    UNION ALL
    {right_generations}
),
ranked_upstream_models AS (
    SELECT upstream_account_id, model
      FROM source
     WHERE upstream_account_id <> ''
     GROUP BY upstream_account_id, model
     ORDER BY SUM(requests) DESC, upstream_account_id ASC, model ASC
     LIMIT {TOP_UPSTREAM_MODEL_LIMIT}
),
summary_rows AS (
    SELECT 'summary' AS kind, '' AS upstream_account_id, '' AS model, currency,
           {metric_sums}
      FROM source
     GROUP BY currency
),
top_rows AS (
    SELECT 'top' AS kind, activity.upstream_account_id, activity.model, activity.currency,
           {metric_sums}
      FROM source activity
      JOIN ranked_upstream_models ranked
        ON ranked.upstream_account_id = activity.upstream_account_id
       AND ranked.model = activity.model
     GROUP BY activity.upstream_account_id, activity.model, activity.currency
)
SELECT * FROM summary_rows
UNION ALL
SELECT * FROM top_rows
ORDER BY kind ASC, upstream_account_id ASC, model ASC, currency ASC"#,
        metric_sums = metric_sums(),
    )
}

/// A single terminal-fact probe for the freshness watermark. The two branches
/// are independently indexed by their fact time and never join request bodies.
pub(super) fn monitoring_freshness_sql(scope: &MonitoringScope) -> String {
    let predicate = match scope {
        MonitoringScope::Tenant(_) => {
            "f.tenant_id = $1 AND f.created_at >= $2 AND f.created_at <= $3"
        }
        MonitoringScope::Global => "f.created_at >= $1 AND f.created_at <= $2",
    };
    format!(
        r#"SELECT created_at
             FROM (
                   SELECT created_at
                     FROM (
                           SELECT f.created_at FROM request_stats_facts f
                            WHERE {predicate}
                            ORDER BY f.created_at DESC
                            LIMIT 1
                     ) request_terminal
                   UNION ALL
                   SELECT created_at
                     FROM (
                           SELECT f.created_at FROM generation_stats_facts f
                            WHERE {predicate}
                            ORDER BY f.created_at DESC
                            LIMIT 1
                     ) generation_terminal
             ) terminal
            ORDER BY created_at DESC
            LIMIT 1"#,
    )
}

/// Five terminal outcomes for one already-ranked stable upstream/model pair.
/// `request_stats_facts` and `generation_stats_facts` are populated only after
/// their respective lifecycles complete, excluding all started traffic.
pub(super) fn monitoring_terminal_outcomes_sql(scope: &MonitoringScope) -> String {
    let predicate = match scope {
        MonitoringScope::Tenant(_) => {
            "f.tenant_id = $1 AND f.upstream_account_id = $2 AND f.model = $3 AND f.created_at >= $4 AND f.created_at <= $5"
        }
        MonitoringScope::Global => {
            "f.upstream_account_id = $1 AND f.model = $2 AND f.created_at >= $3 AND f.created_at <= $4"
        }
    };
    format!(
        r#"SELECT id, source, created_at, status_class, duration_ms, error_code
             FROM (
                   SELECT id, source, created_at, status_class, duration_ms, error_code
                     FROM (
                           SELECT f.request_id AS id, 'request' AS source, f.created_at,
                                  f.status_class, f.duration_ms, f.error_code
                             FROM request_stats_facts f
                            WHERE {predicate}
                            ORDER BY f.created_at DESC, f.request_id DESC
                            LIMIT {TERMINAL_OUTCOME_LIMIT}
                     ) request_terminal
                   UNION ALL
                   SELECT id, source, created_at, status_class, duration_ms, error_code
                     FROM (
                           SELECT f.job_id AS id, 'generation' AS source, f.created_at,
                                  f.status_class, f.duration_ms, f.error_code
                             FROM generation_stats_facts f
                            WHERE {predicate}
                            ORDER BY f.created_at DESC, f.job_id DESC
                            LIMIT {TERMINAL_OUTCOME_LIMIT}
                     ) generation_terminal
             ) terminal
            ORDER BY created_at DESC, id DESC
            LIMIT {TERMINAL_OUTCOME_LIMIT}"#,
    )
}

fn request_fact_sql(from_parameter: &str, to_parameter: &str, scope_predicate: &str) -> String {
    format!(
        r#"SELECT f.upstream_account_id, f.model, f.status_class, f.currency,
                  CAST(1 AS BIGINT) AS requests,
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
                  f.cost_micros
             FROM request_stats_facts f
            WHERE {from_parameter} <= {to_parameter}
              AND f.created_at >= {from_parameter}
              AND f.created_at <= {to_parameter}
              AND {scope_predicate}"#,
    )
}

fn generation_fact_sql(from_parameter: &str, to_parameter: &str, scope_predicate: &str) -> String {
    format!(
        r#"SELECT f.upstream_account_id, f.model, f.status_class, f.currency,
                  CAST(1 AS BIGINT) AS requests,
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
                  f.cost_micros
             FROM generation_stats_facts f
            WHERE {from_parameter} <= {to_parameter}
              AND f.created_at >= {from_parameter}
              AND f.created_at <= {to_parameter}
              AND {scope_predicate}"#,
    )
}

fn metric_sums() -> &'static str {
    r#"CAST(COALESCE(SUM(requests), 0) AS BIGINT) AS requests,
       CAST(COALESCE(SUM(CASE WHEN status_class = 'success' THEN requests ELSE 0 END), 0) AS BIGINT) AS successful_requests,
       CAST(COALESCE(SUM(CASE WHEN status_class = 'failure' THEN requests ELSE 0 END), 0) AS BIGINT) AS failed_requests,
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

    #[test]
    fn bounded_snapshot_sql_uses_rollups_terminal_facts_and_top_n_only() {
        for scope in [
            MonitoringScope::Tenant("tenant".to_owned()),
            MonitoringScope::Global,
        ] {
            for granularity in [MonitoringGranularity::Hour, MonitoringGranularity::Day] {
                let sql = monitoring_snapshot_sql(&scope, granularity);
                assert!(sql.contains("usage_analysis_"), "{sql}");
                assert!(sql.contains("request_stats_facts"), "{sql}");
                assert!(sql.contains("generation_stats_facts"), "{sql}");
                assert!(!sql.contains("request_records"), "{sql}");
                assert!(!sql.contains("generation_jobs"), "{sql}");
                assert!(sql.contains("LIMIT 10"), "{sql}");
            }
            let outcomes = monitoring_terminal_outcomes_sql(&scope);
            assert!(outcomes.contains("LIMIT 5"), "{outcomes}");
            assert!(!outcomes.contains("request_records"), "{outcomes}");
            assert!(!outcomes.contains("generation_jobs"), "{outcomes}");
        }
    }
}
