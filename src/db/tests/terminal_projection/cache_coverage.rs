use super::*;

async fn coverage_contract(database: &Database) {
    for metered in [false, true] {
        for deferred in [false, true] {
            for read in [None, Some(0), Some(2)] {
                let (key, reservation, request_id) = admitted(database, metered).await;
                let mut input = finish(&key, &reservation, request_id);
                let cached = read.unwrap_or(0);
                input.usage.input_tokens = 7 - cached;
                input.usage.cached_input_tokens = cached;
                input.usage.cache_coverage =
                    read.and_then(|read| crate::model::CacheUsageCoverage::new(read, 7));
                if deferred {
                    database
                        .finish_proxy_request_deferred(input.clone())
                        .await
                        .unwrap();
                    assert!(matches!(
                        database.finish_proxy_request_deferred(input).await.unwrap(),
                        FinishProxyRequestResult::AlreadyFinished { .. }
                    ));
                    let owner = Uuid::now_v7();
                    assert_eq!(
                        database
                            .claim_terminal_projection_tasks(owner, 32)
                            .await
                            .unwrap(),
                        vec![request_id]
                    );
                    assert!(
                        database
                            .project_claimed_terminal_projection_task(owner, request_id)
                            .await
                            .unwrap()
                    );
                    assert!(
                        !database
                            .project_claimed_terminal_projection_task(owner, request_id)
                            .await
                            .unwrap()
                    );
                } else {
                    database.finish_proxy_request(input.clone()).await.unwrap();
                    assert!(matches!(
                        database.finish_proxy_request(input).await.unwrap(),
                        FinishProxyRequestResult::AlreadyFinished { .. }
                    ));
                }
                let row = sqlx::query("SELECT created_at, input_tokens, cached_input_tokens, cost_micros, cache_known_read_tokens, cache_known_input_tokens FROM request_stats_facts WHERE request_id = $1")
                    .bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
                let created: i64 = row.get("created_at");
                assert_eq!(row.get::<i64, _>("input_tokens"), 7);
                assert_eq!(row.get::<i64, _>("cached_input_tokens"), cached);
                assert_eq!(row.get::<i64, _>("cost_micros"), 10);
                assert_eq!(row.get::<Option<i64>, _>("cache_known_read_tokens"), read);
                assert_eq!(
                    row.get::<Option<i64>, _>("cache_known_input_tokens"),
                    read.map(|_| 7)
                );
                let day = created / 86_400_000 * 86_400_000;
                for mut filter in [
                    StatsFilter {
                        from_created_at: Some(created),
                        to_created_at: Some(created),
                        ..Default::default()
                    },
                    StatsFilter {
                        from_created_at: Some(day),
                        to_created_at: Some(day + 86_400_000 - 1),
                        ..Default::default()
                    },
                    StatsFilter {
                        from_created_at: Some(day - 1),
                        to_created_at: Some(day + 86_400_000),
                        model: Some(key.alias.clone()),
                        protocol: Some("openai".into()),
                        status: Some("success".into()),
                        min_duration_ms: Some(10),
                        max_duration_ms: Some(10),
                        min_cost_micros: Some(10),
                        max_cost_micros: Some(10),
                        ..Default::default()
                    },
                ] {
                    filter.key_id = Some(Uuid::now_v7());
                    let stats = database
                        .stats_filtered(key.key_id, filter.clone())
                        .await
                        .unwrap();
                    assert_eq!(stats.summary.total_requests, 1);
                    let cache = &stats.summary.cache_usage;
                    assert_eq!(cache.known_read_tokens, cached);
                    assert_eq!(cache.known_input_tokens, if read.is_some() { 7 } else { 0 });
                    assert_eq!(cache.eligible_requests, i64::from(read.is_some()));
                    assert_eq!(cache.unknown_requests, i64::from(read.is_none()));
                    assert_eq!(cache.hit_rate, read.map(|read| read as f64 / 7.0));
                    filter.key_id = Some(key.key_id);
                    let foreign = database
                        .operator_stats_filtered("foreign-cache-tenant", filter.clone())
                        .await
                        .unwrap();
                    assert_eq!(foreign.summary.total_requests, 0);
                    assert_eq!(foreign.summary.cache_usage.hit_rate, None);
                    for excluded in [
                        StatsFilter {
                            model: Some("absent-model".into()),
                            ..filter.clone()
                        },
                        StatsFilter {
                            protocol: Some("generation".into()),
                            ..filter.clone()
                        },
                        StatsFilter {
                            status: Some("pending".into()),
                            ..filter.clone()
                        },
                        StatsFilter {
                            error_code: Some("absent-error".into()),
                            ..filter.clone()
                        },
                        StatsFilter {
                            upstream_account_id: Some(Uuid::now_v7()),
                            ..filter.clone()
                        },
                        StatsFilter {
                            route_id: Some(Uuid::now_v7()),
                            ..filter.clone()
                        },
                        StatsFilter {
                            min_duration_ms: Some(11),
                            max_duration_ms: None,
                            ..filter.clone()
                        },
                        StatsFilter {
                            min_cost_micros: Some(11),
                            max_cost_micros: None,
                            ..filter.clone()
                        },
                        StatsFilter {
                            key_alias: Some("absent-key".into()),
                            ..filter.clone()
                        },
                        StatsFilter {
                            principal: Some("absent-principal".into()),
                            ..filter.clone()
                        },
                    ] {
                        assert_eq!(
                            database
                                .stats_filtered(key.key_id, excluded)
                                .await
                                .unwrap()
                                .summary
                                .total_requests,
                            0
                        );
                    }
                }
                sqlx::query("INSERT INTO request_stats_facts (request_id, tenant_id, key_id, created_at, model, protocol, status_class, error_code, upstream_account_id, model_route_id, duration_ms, input_tokens, output_tokens, currency, cost_micros) SELECT $1, tenant_id, key_id, created_at, model, protocol, status_class, error_code, upstream_account_id, model_route_id, duration_ms, input_tokens, output_tokens, currency, cost_micros FROM request_stats_facts WHERE request_id = $2")
                    .bind(Uuid::now_v7().to_string()).bind(request_id.to_string()).execute(&database.pool).await.unwrap();
                sqlx::query("UPDATE request_daily_aggregates SET cache_unknown_requests = COALESCE(cache_unknown_requests, requests) + 1, requests = requests + 1 WHERE key_id = $1")
                    .bind(key.key_id.to_string()).execute(&database.pool).await.unwrap();
                for (from, to) in [(created, created), (day, day + 86_400_000 - 1)] {
                    let stats = database
                        .stats_filtered(
                            key.key_id,
                            StatsFilter {
                                from_created_at: Some(from),
                                to_created_at: Some(to),
                                ..Default::default()
                            },
                        )
                        .await
                        .unwrap();
                    assert_eq!(stats.summary.total_requests, 2);
                    assert_eq!(
                        stats.summary.cache_usage.unknown_requests,
                        if read.is_some() { 1 } else { 2 }
                    );
                    assert_eq!(
                        stats.summary.cache_usage.eligible_requests,
                        i64::from(read.is_some())
                    );
                    assert_eq!(
                        stats.summary.cache_usage.hit_rate,
                        read.map(|read| read as f64 / 7.0)
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn sqlite_cache_coverage_terminal_replay_filters_and_daily_edges() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("coverage.db").display()
    ))
    .await
    .unwrap();
    database.migrate().await.unwrap();
    coverage_contract(&database).await;
}

#[tokio::test]
async fn postgres_cache_coverage_terminal_replay_filters_and_daily_edges() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let admin = sqlx::PgPool::connect(&database_url).await.unwrap();
    let schema = format!("cache_coverage_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&database_url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let database = Database::connect_with_max(isolated.as_str(), 8)
        .await
        .unwrap();
    database.migrate().await.unwrap();
    coverage_contract(&database).await;
    database.pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
}
