use super::*;

async fn fixture(database: &Database, tenant: &str) -> (Uuid, Uuid, Uuid) {
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
        for day in 10..13 {
            let model = format!("pricing-model-{index:03}");
            let currency = if day == 11 { "CNY" } else { "USD" };
            let status = if day == 11 { "failure" } else { "success" };
            let error = if day == 11 { "pricing_error" } else { "" };
            let protocol = if day == 11 { "anthropic" } else { "openai" };
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
    for day in 10..13 {
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
    let (key, upstream, route) = fixture(database, &tenant).await;
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
        pricing_stats_sql(&base)
    )))
    .bind(&tenant)
    .bind(key.to_string())
    .bind(base.from_created_at.unwrap())
    .bind(base.to_created_at.unwrap())
    .bind("")
    .bind("")
    .bind("")
    .bind("")
    .bind("")
    .bind("")
    .bind(-1_i64)
    .bind(-1_i64)
    .bind(-1_i64)
    .bind(-1_i64)
    .bind("")
    .bind("")
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
}

#[tokio::test]
async fn postgres_pricing_model_projection_matches_existing_statistics() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect(&url).await.unwrap();
    database.migrate().await.unwrap();
    parity(&database).await;
}

#[test]
fn pricing_query_has_only_model_projection_and_disjoint_indexable_edges() {
    let query = pricing_stats_sql(&StatsFilter::default());
    assert!(!query.contains("GROUPING SETS"));
    assert!(!query.contains("MATERIALIZED"));
    assert!(!query.contains("DENSE_RANK"));
    assert!(!query.contains(EDGE_PREDICATE));
    assert_eq!(query.matches("AND f.created_at < $17").count(), 2);
    assert_eq!(
        query
            .matches("AND f.created_at >= $18 AND f.created_at >= $17")
            .count(),
        2
    );
    assert!(query.contains("ORDER BY calls DESC, model ASC"));
    assert!(query.contains("LIMIT 100"));
}
