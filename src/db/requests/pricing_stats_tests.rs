use super::*;

const FIXTURE_START_DAY: i64 = 10;
const HOT_PATH_START_DAY: i64 = 50_000;

async fn fixture(database: &Database, tenant: &str, start_day: i64) -> (Uuid, Uuid, Uuid) {
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: tenant.to_owned(),
                principal_external_id: "Pricing-Principal".to_owned(),
                alias: "Pricing-Key".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            b"pricing projection fixture pepper sufficiently long",
        )
        .await
        .unwrap();
    let tenant_id: String = sqlx::query_scalar("SELECT tenant_id FROM key_records WHERE id = $1")
        .bind(issued.key_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
    let upstream = Uuid::now_v7();
    let route = Uuid::now_v7();
    // More than 100 equally-ranked models exercises stable top-100 ordering.
    // Each has facts and matching rollups in all three days and two currencies.
    for index in 0..103 {
        for day in start_day..start_day + 3 {
            let model = format!("pricing-model-{index:03}");
            let middle_day = day == start_day + 1;
            let currency = if middle_day { "CNY" } else { "USD" };
            let status = if middle_day { "failure" } else { "success" };
            let error = if middle_day { "pricing_error" } else { "" };
            let protocol = if middle_day { "anthropic" } else { "openai" };
            sqlx::query(
                "INSERT INTO request_stats_facts (request_id, tenant_id, key_id, created_at, model, protocol, status_class, error_code, upstream_account_id, model_route_id, duration_ms, input_tokens, output_tokens, currency, cost_micros) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,15,4,7,$11,100)",
            ).bind(Uuid::now_v7().to_string()).bind(&tenant_id).bind(issued.key_id.to_string())
                .bind(day * DAY_MILLIS + 100).bind(&model).bind(protocol).bind(status).bind(error)
                .bind(upstream.to_string()).bind(route.to_string()).bind(currency)
                .execute(&database.pool).await.unwrap();
            sqlx::query(
                "INSERT INTO request_daily_aggregates (tenant_id,key_id,day_bucket,model,protocol,status_class,error_code,upstream_account_id,model_route_id,currency,requests,input_tokens,output_tokens,cost_micros) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,1,4,7,100)",
            ).bind(&tenant_id).bind(issued.key_id.to_string()).bind(day).bind(&model)
                .bind(protocol).bind(status).bind(error).bind(upstream.to_string()).bind(route.to_string())
                .bind(currency).execute(&database.pool).await.unwrap();
        }
    }
    for day in start_day..start_day + 3 {
        sqlx::query(
            "INSERT INTO generation_stats_facts (job_id,tenant_id,key_id,created_at,model,status_class,error_code,upstream_account_id,duration_ms,cost_micros,billed_units,currency) VALUES ($1,$2,$3,$4,'pricing-image','success','',$5,25,200,1,'USD')",
        ).bind(Uuid::now_v7().to_string()).bind(&tenant_id).bind(issued.key_id.to_string())
            .bind(day * DAY_MILLIS + 100).bind(upstream.to_string()).execute(&database.pool).await.unwrap();
        sqlx::query(
            "INSERT INTO generation_daily_aggregates (tenant_id,key_id,day_bucket,model,status_class,error_code,upstream_account_id,requests,billed_units,cost_micros,currency) VALUES ($1,$2,$3,'pricing-image','success','',$4,1,1,200,'USD')",
        ).bind(&tenant_id).bind(issued.key_id.to_string()).bind(day).bind(upstream.to_string())
            .execute(&database.pool).await.unwrap();
    }
    (issued.key_id, upstream, route)
}

async fn parity(database: &Database) {
    let tenant = format!("pricing-parity-{}", Uuid::now_v7());
    let (key, upstream, route) = fixture(database, &tenant, FIXTURE_START_DAY).await;
    let base = StatsFilter {
        from_created_at: Some(10 * DAY_MILLIS + 50),
        to_created_at: Some(12 * DAY_MILLIS + 150),
        ..StatsFilter::default()
    };
    let variants = vec![
        base.clone(),
        StatsFilter {
            model: Some("pricing-model-050".into()),
            ..base.clone()
        },
        StatsFilter {
            protocol: Some("generation".into()),
            ..base.clone()
        },
        StatsFilter {
            protocol: Some("anthropic".into()),
            ..base.clone()
        },
        StatsFilter {
            status: Some("error".into()),
            ..base.clone()
        },
        StatsFilter {
            status: Some("success".into()),
            ..base.clone()
        },
        StatsFilter {
            status: Some("pending".into()),
            ..base.clone()
        },
        StatsFilter {
            error_code: Some("pricing_error".into()),
            ..base.clone()
        },
        StatsFilter {
            upstream_account_id: Some(upstream),
            ..base.clone()
        },
        StatsFilter {
            route_id: Some(route),
            ..base.clone()
        },
        StatsFilter {
            min_duration_ms: Some(10),
            max_duration_ms: Some(20),
            ..base.clone()
        },
        StatsFilter {
            min_cost_micros: Some(50),
            max_cost_micros: Some(150),
            ..base.clone()
        },
        StatsFilter {
            key_alias: Some("pricing".into()),
            principal: Some("pricing".into()),
            ..base.clone()
        },
        StatsFilter {
            key_alias: Some("absent".into()),
            ..base.clone()
        },
        StatsFilter {
            from_created_at: Some(10 * DAY_MILLIS + 100),
            to_created_at: Some(10 * DAY_MILLIS + 100),
            ..base.clone()
        },
        StatsFilter {
            from_created_at: Some(10 * DAY_MILLIS),
            to_created_at: Some(13 * DAY_MILLIS - 1),
            ..base.clone()
        },
        StatsFilter {
            from_created_at: Some(10 * DAY_MILLIS + 101),
            to_created_at: Some(11 * DAY_MILLIS + 99),
            ..base.clone()
        },
    ];
    for filter in variants {
        for selected_tenant in [Some(tenant.as_str()), None, Some("pricing-absent-tenant")] {
            // Global scope is still keyed so concurrent PostgreSQL tests do not
            // change this fixture's result while the two queries are compared.
            let filter = StatsFilter {
                key_id: Some(key),
                ..filter.clone()
            };
            let expected = match selected_tenant {
                Some(tenant) => {
                    database
                        .operator_stats_filtered(tenant, filter.clone())
                        .await
                }
                None => {
                    database
                        .global_operator_stats_filtered(filter.clone())
                        .await
                }
            }
            .unwrap()
            .by_model
            .into_iter()
            .map(|row| (row.name, row.requests, row.input_tokens, row.output_tokens))
            .collect::<Vec<_>>();
            assert_eq!(
                database
                    .pricing_model_usage(selected_tenant, filter)
                    .await
                    .unwrap(),
                expected
            );
        }
    }
    assert_eq!(
        database
            .pricing_model_usage(Some(&tenant), base.clone())
            .await
            .unwrap()
            .len(),
        100
    );
    // CI-only EXPLAIN for the actual projection SQL. The fixtures contain no
    // secret/user input; every value below remains a driver-bound parameter.
    let prefix = match database.backend {
        DatabaseBackend::PostgreSql => "EXPLAIN ",
        DatabaseBackend::Sqlite => "EXPLAIN QUERY PLAN ",
    };
    let plan = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{prefix}{}",
        pricing_stats_sql(None, &base)
    )))
    .bind(base.from_created_at.unwrap())
    .bind(base.to_created_at.unwrap())
    .bind(11 * DAY_MILLIS)
    .bind(12 * DAY_MILLIS)
    .fetch_all(&database.pool)
    .await
    .unwrap();
    assert!(!plan.is_empty());
    if matches!(database.backend, DatabaseBackend::Sqlite) {
        let details = plan
            .iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect::<Vec<_>>();
        assert!(details.iter().any(|detail| detail.contains("INDEX")));
    } else {
        let details = plan
            .iter()
            .map(|row| row.get::<String, _>(0))
            .collect::<Vec<_>>();
        assert!(details.iter().any(|detail| detail.contains("Limit")));
        assert!(!details.iter().any(|detail| detail.contains("WindowAgg")));
    }
    for invalid in [
        StatsFilter::default(),
        StatsFilter {
            from_created_at: Some(-1),
            ..base.clone()
        },
        StatsFilter {
            to_created_at: Some(0),
            ..base.clone()
        },
        StatsFilter {
            to_created_at: Some(200 * DAY_MILLIS),
            ..base.clone()
        },
        StatsFilter {
            status: Some("invalid".into()),
            ..base.clone()
        },
        StatsFilter {
            min_duration_ms: Some(-1),
            ..base.clone()
        },
    ] {
        assert!(
            database
                .pricing_model_usage(Some(&tenant), invalid)
                .await
                .is_err()
        );
    }
}

async fn hot_path_parity(database: &Database) {
    let tenant = format!("pricing-hot-path-{}", Uuid::now_v7());
    let (key, _, _) = fixture(database, &tenant, HOT_PATH_START_DAY).await;
    let base = StatsFilter {
        from_created_at: Some(HOT_PATH_START_DAY * DAY_MILLIS + 50),
        to_created_at: Some((HOT_PATH_START_DAY + 2) * DAY_MILLIS + 150),
        ..StatsFilter::default()
    };
    // No tenant/key/other filter is present in these comparisons, so every
    // pricing read uses the global specialized query. The fixture has request
    // and generation facts, >100 models, one complete daily bucket, and two
    // exact fact edges.
    for filter in [
        base.clone(),
        StatsFilter {
            from_created_at: Some(HOT_PATH_START_DAY * DAY_MILLIS + 100),
            to_created_at: Some(HOT_PATH_START_DAY * DAY_MILLIS + 100),
            ..base.clone()
        },
        StatsFilter {
            from_created_at: Some(HOT_PATH_START_DAY * DAY_MILLIS),
            to_created_at: Some((HOT_PATH_START_DAY + 3) * DAY_MILLIS - 1),
            ..base.clone()
        },
        StatsFilter {
            from_created_at: Some(HOT_PATH_START_DAY * DAY_MILLIS + 101),
            to_created_at: Some((HOT_PATH_START_DAY + 1) * DAY_MILLIS + 99),
            ..base.clone()
        },
    ] {
        assert_global_unfiltered_parity(database, filter).await;
    }

    if matches!(database.backend, DatabaseBackend::Sqlite) {
        // The core history tables have no key/principal foreign keys. The
        // authoritative joins hide such stale facts, and the specialized
        // eligible-key CTE must do the same.
        let principal_id: String =
            sqlx::query_scalar("SELECT principal_id FROM key_records WHERE id = $1")
                .bind(key.to_string())
                .fetch_one(&database.pool)
                .await
                .unwrap();
        sqlx::query("DELETE FROM principals WHERE id = $1")
            .bind(principal_id)
            .execute(&database.pool)
            .await
            .unwrap();
        assert_global_unfiltered_parity(database, base.clone()).await;

        let deleted_key_tenant = format!("pricing-deleted-key-{}", Uuid::now_v7());
        let (deleted_key, _, _) = fixture(database, &deleted_key_tenant, HOT_PATH_START_DAY).await;
        sqlx::query("DELETE FROM key_records WHERE id = $1")
            .bind(deleted_key.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        assert_global_unfiltered_parity(database, base).await;
    }
}

async fn assert_global_unfiltered_parity(database: &Database, filter: StatsFilter) {
    let expected = database
        .global_operator_stats_filtered(filter.clone())
        .await
        .unwrap()
        .by_model
        .into_iter()
        .map(|row| (row.name, row.requests, row.input_tokens, row.output_tokens))
        .collect::<Vec<_>>();
    assert_eq!(
        database.pricing_model_usage(None, filter).await.unwrap(),
        expected
    );
}

#[tokio::test]
async fn sqlite_pricing_model_projection_matches_existing_statistics() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("pricing.db").display()
    ))
    .await
    .unwrap();
    database.migrate().await.unwrap();
    parity(&database).await;
    hot_path_parity(&database).await;
}

#[tokio::test]
async fn postgres_pricing_model_projection_matches_existing_statistics() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect(&url).await.unwrap();
    database.migrate().await.unwrap();
    parity(&database).await;
    hot_path_parity(&database).await;
}

#[test]
fn pricing_query_has_only_model_projection_and_disjoint_indexable_edges() {
    let query = pricing_stats_sql(None, &StatsFilter::default());
    assert!(!query.contains("GROUPING SETS"));
    assert!(!query.contains("filtered_activity AS MATERIALIZED"));
    assert!(!query.contains("DENSE_RANK"));
    // The durable identity relation is built once from retained facts; the six
    // activity arms only join its compact `(key_id, tenant_id)` projection.
    assert_eq!(
        query
            .matches("pricing_visible_keys AS MATERIALIZED")
            .count(),
        1
    );
    assert_eq!(query.matches("FROM key_records").count(), 0);
    assert_eq!(query.matches("JOIN principals").count(), 0);
    assert_eq!(query.matches("JOIN tenants").count(), 0);
    assert_eq!(query.matches("UNION SELECT key_id, tenant_id").count(), 3);
    assert_eq!(query.matches("JOIN pricing_visible_keys").count(), 6);
    assert_eq!(query.matches("AND f.created_at < $3").count(), 2);
    assert_eq!(
        query
            .matches("AND f.created_at >= $4 AND f.created_at >= $3")
            .count(),
        2
    );
    assert!(query.contains("ORDER BY calls DESC, model ASC"));
    assert!(query.contains("LIMIT 100"));

    let scoped_or_filtered = pricing_stats_sql(
        Some("tenant-a"),
        &StatsFilter {
            model: Some("model-a".to_owned()),
            ..StatsFilter::default()
        },
    );
    assert!(scoped_or_filtered.contains("JOIN key_records"));
    assert!(!scoped_or_filtered.contains(EDGE_PREDICATE));
    assert_eq!(
        scoped_or_filtered.matches("AND f.created_at < $17").count(),
        2
    );
    assert_eq!(
        scoped_or_filtered
            .matches("AND f.created_at >= $18 AND f.created_at >= $17")
            .count(),
        2
    );
}
