use super::{MonitoringGranularity, MonitoringScope};

pub(super) const TOP_UPSTREAM_MODEL_LIMIT: usize = 10;
pub(super) const TERMINAL_OUTCOME_LIMIT: usize = 5;

#[derive(Clone, Copy)]
pub(super) enum MonitoringTerminalBatchDialect {
    PostgreSql,
    Sqlite,
}

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

/// Resolve current-account and retained deleted-account identity/health for a
/// bounded set of already-ranked accounts in one statement.
pub(super) fn monitoring_upstream_health_batch_sql(account_count: usize) -> Option<String> {
    let selected = selected_values(account_count, 1)?;
    Some(format!(
        r#"WITH selected(upstream_account_id) AS (VALUES {selected})
SELECT target.upstream_account_id,
       COALESCE(account.name, deleted.name, target.upstream_account_id) AS name,
       account.status, account.credential_generation,
       health.credential_generation AS health_generation,
       health.consecutive_failures, health.cooldown_until, health.updated_at
  FROM selected target
  LEFT JOIN upstream_accounts account
         ON account.id = target.upstream_account_id
  LEFT JOIN deleted_upstream_account_snapshots deleted
         ON deleted.upstream_account_id = target.upstream_account_id
  LEFT JOIN upstream_account_health health
         ON health.upstream_account_id = account.id
 ORDER BY target.upstream_account_id ASC"#,
    ))
}

/// Fetch the five newest terminal outcomes for every selected pair in one
/// statement. PostgreSQL uses two bounded LATERAL index probes per pair so a
/// large selected time window cannot turn this drilldown into a fact scan.
/// SQLite uses two bounded CTE index probes per pair and unions their bounded
/// results, retaining the same one-statement and five-newest contract.
pub(super) fn monitoring_terminal_outcomes_batch_sql(
    scope: &MonitoringScope,
    pair_count: usize,
    dialect: MonitoringTerminalBatchDialect,
) -> Option<String> {
    if pair_count == 0 || pair_count > TOP_UPSTREAM_MODEL_LIMIT {
        return None;
    }
    let first_scope_parameter = pair_count.checked_mul(2)?.checked_add(1)?;
    let (tenant_predicate, from_parameter, to_parameter) = match scope {
        MonitoringScope::Tenant(_) => (
            format!("AND f.tenant_id = ${first_scope_parameter}"),
            first_scope_parameter.checked_add(1)?,
            first_scope_parameter.checked_add(2)?,
        ),
        MonitoringScope::Global => (
            String::new(),
            first_scope_parameter,
            first_scope_parameter.checked_add(1)?,
        ),
    };
    let sql = match dialect {
        MonitoringTerminalBatchDialect::PostgreSql => {
            let selected = selected_values(pair_count, 2)?;
            format!(
                r#"WITH selected(upstream_account_id, model) AS (VALUES {selected})
SELECT target.upstream_account_id, target.model,
       terminal.id, terminal.source, terminal.created_at, terminal.status_class,
       terminal.duration_ms, terminal.error_code
  FROM selected target
 CROSS JOIN LATERAL (
       SELECT candidate.id, candidate.source, candidate.created_at,
              candidate.status_class, candidate.duration_ms, candidate.error_code
         FROM (
              (SELECT f.request_id AS id, 'request' AS source, f.created_at,
                      f.status_class, f.duration_ms, f.error_code
                FROM request_stats_facts f
                WHERE f.upstream_account_id = target.upstream_account_id
                  AND f.upstream_account_id <> ''
                  AND f.model = target.model
                  {tenant_predicate}
                  AND f.created_at >= ${from_parameter}
                  AND f.created_at <= ${to_parameter}
                ORDER BY f.created_at DESC, f.request_id DESC
                LIMIT {TERMINAL_OUTCOME_LIMIT})
              UNION ALL
              (SELECT f.job_id AS id, 'generation' AS source, f.created_at,
                      f.status_class, f.duration_ms, f.error_code
                FROM generation_stats_facts f
                WHERE f.upstream_account_id = target.upstream_account_id
                  AND f.upstream_account_id <> ''
                  AND f.model = target.model
                  {tenant_predicate}
                  AND f.created_at >= ${from_parameter}
                  AND f.created_at <= ${to_parameter}
                ORDER BY f.created_at DESC, f.job_id DESC
                LIMIT {TERMINAL_OUTCOME_LIMIT})
         ) candidate
        ORDER BY candidate.created_at DESC, candidate.id DESC
        LIMIT {TERMINAL_OUTCOME_LIMIT}
  ) terminal
 ORDER BY target.upstream_account_id ASC, target.model ASC,
          terminal.created_at DESC, terminal.id DESC"#,
            )
        }
        MonitoringTerminalBatchDialect::Sqlite => sqlite_bounded_terminal_batch_sql(
            pair_count,
            &tenant_predicate,
            from_parameter,
            to_parameter,
        )?,
    };
    Some(sql)
}

fn sqlite_bounded_terminal_batch_sql(
    pair_count: usize,
    tenant_predicate: &str,
    from_parameter: usize,
    to_parameter: usize,
) -> Option<String> {
    if pair_count == 0 || pair_count > TOP_UPSTREAM_MODEL_LIMIT {
        return None;
    }
    let mut ctes = Vec::with_capacity(pair_count.checked_mul(3)?);
    let mut pair_selects = Vec::with_capacity(pair_count);
    for index in 0..pair_count {
        let upstream_parameter = index.checked_mul(2)?.checked_add(1)?;
        let model_parameter = upstream_parameter.checked_add(1)?;
        ctes.push(format!(
            r#"request_{index} AS (
    SELECT f.request_id AS id, 'request' AS source, f.created_at,
           f.status_class, f.duration_ms, f.error_code
      FROM request_stats_facts f
     WHERE f.upstream_account_id = ${upstream_parameter}
       AND f.upstream_account_id <> ''
       AND f.model = ${model_parameter}
       {tenant_predicate}
       AND f.created_at >= ${from_parameter}
       AND f.created_at <= ${to_parameter}
     ORDER BY f.created_at DESC, f.request_id DESC
     LIMIT {TERMINAL_OUTCOME_LIMIT}
)"#,
        ));
        ctes.push(format!(
            r#"generation_{index} AS (
    SELECT f.job_id AS id, 'generation' AS source, f.created_at,
           f.status_class, f.duration_ms, f.error_code
      FROM generation_stats_facts f
     WHERE f.upstream_account_id = ${upstream_parameter}
       AND f.upstream_account_id <> ''
       AND f.model = ${model_parameter}
       {tenant_predicate}
       AND f.created_at >= ${from_parameter}
       AND f.created_at <= ${to_parameter}
     ORDER BY f.created_at DESC, f.job_id DESC
     LIMIT {TERMINAL_OUTCOME_LIMIT}
)"#,
        ));
        ctes.push(format!(
            r#"pair_{index} AS (
    SELECT ${upstream_parameter} AS upstream_account_id,
           ${model_parameter} AS model,
           candidate.id, candidate.source, candidate.created_at,
           candidate.status_class, candidate.duration_ms, candidate.error_code
      FROM (
            SELECT * FROM request_{index}
            UNION ALL
            SELECT * FROM generation_{index}
      ) candidate
     ORDER BY candidate.created_at DESC, candidate.id DESC
     LIMIT {TERMINAL_OUTCOME_LIMIT}
)"#,
        ));
        pair_selects.push(format!("SELECT * FROM pair_{index}"));
    }
    Some(format!(
        "WITH {}\n{}\nORDER BY upstream_account_id ASC, model ASC, created_at DESC, id DESC",
        ctes.join(",\n"),
        pair_selects.join("\nUNION ALL\n")
    ))
}

fn selected_values(row_count: usize, columns_per_row: usize) -> Option<String> {
    if row_count == 0 || row_count > TOP_UPSTREAM_MODEL_LIMIT || columns_per_row == 0 {
        return None;
    }
    let mut parameter = 1_usize;
    let mut rows = Vec::with_capacity(row_count);
    for _ in 0..row_count {
        let mut columns = Vec::with_capacity(columns_per_row);
        for _ in 0..columns_per_row {
            columns.push(format!("${parameter}"));
            parameter = parameter.checked_add(1)?;
        }
        rows.push(format!("({})", columns.join(", ")));
    }
    Some(rows.join(", "))
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
            for dialect in [
                MonitoringTerminalBatchDialect::PostgreSql,
                MonitoringTerminalBatchDialect::Sqlite,
            ] {
                let outcomes = monitoring_terminal_outcomes_batch_sql(&scope, 10, dialect).unwrap();
                assert!(outcomes.contains("LIMIT 5") || outcomes.contains("outcome_rank <= 5"));
                assert!(!outcomes.contains("request_records"), "{outcomes}");
                assert!(!outcomes.contains("generation_jobs"), "{outcomes}");
            }
        }
    }
}
