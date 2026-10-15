use std::collections::BTreeMap;

use sqlx::Row;

#[test]
fn p95_disambiguates_finite_upper_bound_and_open_ended_bucket() {
    use super::{approximate_p95, histogram_p95_is_capped};
    let mut buckets = [0_i64; 12];
    assert!(!histogram_p95_is_capped(0, &buckets));
    buckets[10] = 95;
    buckets[11] = 5;
    assert_eq!(approximate_p95(100, &buckets), Some(60_000));
    assert!(!histogram_p95_is_capped(100, &buckets));
    buckets[10] = 94;
    buckets[11] = 6;
    assert_eq!(approximate_p95(100, &buckets), Some(60_000));
    assert!(histogram_p95_is_capped(100, &buckets));
    let metrics = UsageMetricsAccumulator {
        duration_count: 100,
        duration_buckets: buckets,
        ..Default::default()
    }
    .finish();
    assert!(metrics.p95_is_capped);
    let unavailable = UsageMetricsAccumulator {
        duration_count: 100,
        duration_buckets: buckets,
        ..Default::default()
    }
    .finish_without_p95();
    assert_eq!(unavailable.p95_duration_ms, None);
    assert!(!unavailable.p95_is_capped);
}

use super::{
    Database, DatabaseBackend, UsageAnalysisBucketPlan, UsageAnalysisGranularity,
    UsageMetricsAccumulator, generation_usage_dimension_sql, session_usage_dimension_sql,
    usage_analysis_heatmap_sql, usage_analysis_main_sql,
};

#[test]
fn historical_usage_survives_product_identity_purge() {
    let main = usage_analysis_main_sql(
        DatabaseBackend::PostgreSql,
        UsageAnalysisGranularity::Day,
        true,
    );
    let sessions = session_usage_dimension_sql(UsageAnalysisGranularity::Day, true);
    let generation = generation_usage_dimension_sql(UsageAnalysisGranularity::Day, true);
    for sql in [main, sessions, generation] {
        assert!(sql.contains("LEFT JOIN key_records"), "{sql}");
        assert!(sql.contains("LEFT JOIN principals"), "{sql}");
        assert!(sql.contains("__retired_credential__"), "{sql}");
        assert!(sql.contains("__retired_principal__"), "{sql}");
    }
}

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
        // Both request facts and asynchronous generation facts contribute
        // bounded leading/trailing edge rows around the materialized bucket.
        assert_eq!(sql.matches("fact.created_at >=").count(), 4, "{sql}");
        assert_eq!(sql.matches("fact.tenant_id = $1").count(), 4, "{sql}");
        assert!(
            sql.contains("GROUP BY key_id, key_alias, session_id, currency"),
            "{sql}"
        );
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

#[test]
fn output_rate_retained_lookups_keep_scope_time_and_provenance() {
    for granularity in [
        UsageAnalysisGranularity::Hour,
        UsageAnalysisGranularity::Day,
    ] {
        for tenant_scoped in [false, true] {
            let sql = super::output_rate::output_rate_sql(granularity, tenant_scoped);
            assert!(sql.contains("f.created_at >= $2 AND f.created_at <= $3"));
            assert!(sql.contains("r.created_at >= $2 AND r.created_at <= $3"));
            assert!(sql.contains("r.id = f.request_id AND r.created_at = f.created_at"));
            assert!(sql.contains("r.usage_basis = 'provider_reported'"));
            assert!(sql.contains("o.key_id = r.key_id"));
            assert!(sql.contains("o.cluster_id = r.conversation_cluster_id"));
            assert!(sql.contains("o.compaction = 1"));
            assert!(!sql.contains("request_object"));
            assert!(!sql.contains("response_object"));
            assert!(!sql.contains("LATERAL"));
            assert!(!sql.contains("OFFSET"));
        }
    }
}

#[tokio::test]
async fn postgres_output_rate_30d_prunes_history_and_plans_dense_set_join() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let mut connection = <sqlx::PgConnection as sqlx::Connection>::connect(&database_url)
        .await
        .unwrap();
    sqlx::raw_sql(
        "SET statement_timeout = '30s';
         CREATE TEMP TABLE request_stats_facts (
             request_id TEXT PRIMARY KEY, tenant_id TEXT, key_id TEXT, created_at BIGINT,
             model TEXT, protocol TEXT, error_code TEXT, upstream_account_id TEXT,
             model_route_id TEXT);
         CREATE INDEX ON request_stats_facts (tenant_id, created_at);
         CREATE TEMP TABLE request_records (
             id TEXT, created_at BIGINT, key_id TEXT, conversation_cluster_id TEXT,
             completed_at BIGINT, status_code BIGINT, error_code TEXT, usage_basis TEXT,
             duration_ms BIGINT, output_tokens BIGINT, PRIMARY KEY (id, created_at))
             PARTITION BY RANGE (created_at);
         CREATE TEMP TABLE key_records (id TEXT PRIMARY KEY, tenant_id TEXT,
             principal_id TEXT, alias TEXT);
         CREATE TEMP TABLE principals (id TEXT PRIMARY KEY, tenant_id TEXT, external_id TEXT);
         CREATE TEMP TABLE conversation_observations (request_id TEXT, key_id TEXT,
             cluster_id TEXT, compaction BIGINT);",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    const MONTH: i64 = 30 * 86_400_000;
    for partition in 0..64 {
        let sql = format!(
            "CREATE TEMP TABLE request_records_rate_{partition} PARTITION OF request_records
             FOR VALUES FROM ({}) TO ({})",
            partition * MONTH,
            (partition + 1) * MONTH,
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&mut connection)
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO request_records
         SELECT 'history-' || sequence, (sequence / 1000) * $1 + (sequence % 1000) * 60000,
                'other-key', '', 1, 200, '', 'provider_reported', 1000, 20
           FROM generate_series(0, 63999) sequence",
    )
    .bind(MONTH)
    .execute(&mut connection)
    .await
    .unwrap();
    let from = 30 * MONTH + 12 * 86_400_000 + 12 * 3_600_000;
    let to = from + MONTH;
    for (request_id, created_at) in [
        ("first", from),
        ("middle", from + 3_600_000),
        ("last", to),
        ("before", from - 1),
        ("after", to + 1),
    ] {
        sqlx::query(
            "INSERT INTO request_stats_facts VALUES ($1, 'rate-tenant', 'rate-key', $2,
                 'rate-model', 'openai', '', '', '')",
        )
        .bind(request_id)
        .bind(created_at)
        .execute(&mut connection)
        .await
        .unwrap();
    }
    sqlx::raw_sql(
        "INSERT INTO request_records SELECT request_id, created_at, key_id, '',
             created_at + 1000, 200, '', 'provider_reported', 1000, 20
           FROM request_stats_facts;
         ANALYZE request_stats_facts;
         ANALYZE request_records;",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    let sql = super::output_rate::output_rate_sql(UsageAnalysisGranularity::Hour, true);
    let bind = |sql| {
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind("rate-tenant")
            .bind(from)
            .bind(to)
            .bind("")
            .bind("")
            .bind("")
            .bind("")
            .bind("")
            .bind("")
            .bind("")
            .bind("")
            .bind("")
    };
    let rows = bind(sql.clone()).fetch_all(&mut connection).await.unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| row.get::<i64, _>("requests"))
            .sum::<i64>(),
        3
    );
    assert_eq!(
        rows.iter()
            .map(|row| row.get::<i64, _>("output_tokens"))
            .sum::<i64>(),
        60
    );
    assert_eq!(
        rows.iter()
            .map(|row| row.get::<i64, _>("duration_ms"))
            .sum::<i64>(),
        3000
    );
    let plan_rows = bind(format!("EXPLAIN (ANALYZE, COSTS OFF) {sql}"))
        .fetch_all(&mut connection)
        .await
        .unwrap();
    let plan = plan_rows
        .iter()
        .map(|row| row.get::<String, _>(0))
        .collect::<Vec<_>>()
        .join("\n");
    for partition in 0..64 {
        if ![30, 31].contains(&partition) {
            assert!(
                !plan.contains(&format!("on request_records_rate_{partition} ")),
                "{plan}"
            );
        }
    }

    // Unlike the sparse fixture above, a broad range must allow set joins.
    // Add many matching facts across the selected partitions;
    // compare planner work against the former forced per-fact lookup, without
    // using a wall-clock threshold that depends on the CI runner.
    sqlx::query(
        "INSERT INTO request_stats_facts
         SELECT 'dense-' || sequence, 'rate-tenant', 'rate-key',
                CAST($1 AS BIGINT) + sequence * 60000, 'rate-model', 'openai', '', '', ''
           FROM generate_series(1, 32000) sequence",
    )
    .bind(from)
    .execute(&mut connection)
    .await
    .unwrap();
    sqlx::raw_sql(
        "INSERT INTO request_records SELECT request_id, created_at, key_id, '',
             created_at + 1000, 200, '', 'provider_reported', 1000, 20
           FROM request_stats_facts WHERE request_id LIKE 'dense-%';
         ANALYZE request_stats_facts;
         ANALYZE request_records;",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    for tenant_scoped in [false, true] {
        for granularity in [
            UsageAnalysisGranularity::Hour,
            UsageAnalysisGranularity::Day,
        ] {
            let sql = super::output_rate::output_rate_sql(granularity, tenant_scoped);
            // Global queries require an empty tenant binding, as in production.
            let tenant = if tenant_scoped { "rate-tenant" } else { "" };
            let query = |sql| {
                sqlx::query(sqlx::AssertSqlSafe(sql))
                    .bind(tenant)
                    .bind(from)
                    .bind(to)
                    .bind("rate-key")
                    .bind("")
                    .bind("")
                    .bind("")
                    .bind("")
                    .bind("")
                    .bind("")
                    .bind("")
                    .bind("")
            };
            let rows = query(sql.clone()).fetch_all(&mut connection).await.unwrap();
            for (column, expected) in [
                ("requests", 32003),
                ("output_tokens", 640060),
                ("duration_ms", 32003000),
            ] {
                assert_eq!(
                    rows.iter()
                        .map(|row| row.get::<i64, _>(column))
                        .sum::<i64>(),
                    expected
                );
            }
            let forced = sql.replace(
                "JOIN request_records r ON r.id = f.request_id AND r.created_at = f.created_at\n               AND r.created_at >= $2 AND r.created_at <= $3",
                "JOIN LATERAL (SELECT record.* FROM request_records record
                 WHERE record.id = f.request_id AND record.created_at = f.created_at
                   AND record.created_at >= $2 AND record.created_at <= $3 OFFSET 0) r ON TRUE",
            );
            assert_ne!(forced, sql);
            let mut costs = Vec::new();
            for candidate in [sql, forced] {
                let row = query(format!("EXPLAIN (FORMAT TEXT) {candidate}"))
                    .fetch_one(&mut connection)
                    .await
                    .unwrap();
                let plan: String = row.get(0);
                let total_cost = plan
                    .split("cost=")
                    .nth(1)
                    .unwrap()
                    .split("..")
                    .nth(1)
                    .unwrap()
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .parse::<f64>()
                    .unwrap();
                costs.push(total_cost);
            }
            assert!(
                costs[0] < costs[1],
                "set join must cost less than forced probes: {costs:?}"
            );
        }
    }
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
