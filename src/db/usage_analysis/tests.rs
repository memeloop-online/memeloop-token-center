use std::collections::BTreeMap;

use sqlx::Row;

use super::{
    Database, DatabaseBackend, UsageAnalysisBucketPlan, UsageAnalysisGranularity,
    UsageMetricsAccumulator, session_usage_dimension_sql, usage_analysis_heatmap_sql,
    usage_analysis_main_sql,
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
                assert!(sql.contains("GROUP BY analysis_upstream_id, upstream_label, currency"));
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

#[test]
fn session_dimension_uses_time_indexes_and_never_sums_cost_across_currencies() {
    for (granularity, rollup_table, bucket_column) in [
        (
            UsageAnalysisGranularity::Hour,
            "session_usage_hourly",
            "hour_bucket",
        ),
        (
            UsageAnalysisGranularity::Day,
            "session_usage_daily",
            "day_bucket",
        ),
    ] {
        let sql = session_usage_dimension_sql(granularity, true);
        assert!(
            sql.contains(&format!("FROM {rollup_table} rollup")),
            "{sql}"
        );
        assert!(
            sql.contains(&format!("rollup.{bucket_column} >= $3")),
            "{sql}"
        );
        assert_eq!(sql.matches("fact.created_at >=").count(), 2, "{sql}");
        assert_eq!(sql.matches("fact.tenant_id = $1").count(), 2, "{sql}");
        assert!(sql.contains("GROUP BY session_id, currency"), "{sql}");
        assert!(!sql.contains("SUM(cost_micros) OVER"), "{sql}");
    }
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
        let expected_fact_scans = usize::from(plan.left_from_created_at <= plan.left_to_created_at)
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
