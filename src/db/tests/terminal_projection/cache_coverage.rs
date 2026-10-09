use super::*;

async fn legacy_coverage_contract(database: &Database) {
    let migrations = match database.backend {
        DatabaseBackend::PostgreSql => crate::db::migrations::POSTGRES_MIGRATIONS,
        DatabaseBackend::Sqlite => crate::db::migrations::SQLITE_MIGRATIONS,
    };
    let mut transaction = database.pool.begin().await.unwrap();
    crate::db::migrations::apply_migration_range(&mut transaction, migrations, 1, 121)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    let unique = Uuid::now_v7().to_string();
    let pepper = b"legacy cache coverage contract pepper";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: unique.clone(),
                principal_external_id: "legacy member".into(),
                alias: unique,
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let key = database
        .authenticate_key(&issued.key, pepper)
        .await
        .unwrap();
    sqlx::query("INSERT INTO request_stats_facts (request_id, tenant_id, key_id, created_at, model, protocol, status_class, error_code, upstream_account_id, model_route_id, duration_ms, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, currency, cost_micros) VALUES ($1, $2, $3, 0, 'legacy-cache', 'openai', 'success', '', '', '', 10, 100, 20, 40, 0, 'USD', 120)")
        .bind(Uuid::now_v7().to_string()).bind(key.tenant_id.to_string()).bind(key.key_id.to_string()).execute(&database.pool).await.unwrap();
    sqlx::query("INSERT INTO request_daily_aggregates (tenant_id, key_id, day_bucket, model, protocol, status_class, error_code, upstream_account_id, model_route_id, currency, requests, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, cost_micros) VALUES ($1, $2, 0, 'legacy-cache', 'openai', 'success', '', '', '', 'USD', 5, 500, 100, 200, 0, 600)")
        .bind(key.tenant_id.to_string()).bind(key.key_id.to_string()).execute(&database.pool).await.unwrap();
    database.migrate().await.unwrap();
    for (to, expected) in [(0, 1), (86_399_999, 5)] {
        let stats = database
            .stats_filtered(
                key.key_id,
                StatsFilter {
                    from_created_at: Some(0),
                    to_created_at: Some(to),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(stats.summary.total_requests, expected);
        assert_eq!(stats.summary.input_tokens, expected * 100);
        assert_eq!(stats.summary.cache_usage.unknown_requests, expected);
        assert_eq!(stats.summary.cache_usage.eligible_requests, 0);
        assert_eq!(stats.summary.cache_usage.reported_requests, 0);
        assert_eq!(stats.summary.cache_usage.hit_rate, None);
    }
    let financial: i64 = sqlx::query_scalar(
        "SELECT cached_input_tokens FROM request_daily_aggregates WHERE key_id = $1",
    )
    .bind(key.key_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(financial, 200);
}

async fn coverage_contract(database: &Database) {
    for metered in [false, true] {
        for deferred in [false, true] {
            for (read, denominator) in [
                (None, None),
                (Some(0), Some(7)),
                (Some(2), Some(7)),
                (Some(3), None),
                (Some(0), Some(0)),
            ] {
                let (key, reservation, request_id) = admitted(database, metered).await;
                let mut input = finish(&key, &reservation, request_id);
                let cached = read.unwrap_or(0);
                let financial_input = denominator.unwrap_or(7);
                let eligible = denominator.is_some();
                let rate = denominator
                    .filter(|input| *input > 0)
                    .map(|input| cached as f64 / input as f64);
                let value = if let Some(input) = denominator {
                    serde_json::json!({"usage":{"prompt_tokens":input,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":cached}}})
                } else if read.is_some() {
                    serde_json::json!({"usage":{"input_tokens":financial_input-cached,"output_tokens":3,"cache_read_input_tokens":cached}})
                } else {
                    serde_json::json!({"usage":{"prompt_tokens":financial_input,"completion_tokens":3}})
                };
                input.usage = crate::api::parse_cache_usage_contract(&value);
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
                assert_eq!(row.get::<i64, _>("input_tokens"), financial_input);
                assert_eq!(row.get::<i64, _>("cached_input_tokens"), cached);
                assert_eq!(row.get::<i64, _>("cost_micros"), financial_input + 3);
                assert_eq!(row.get::<Option<i64>, _>("cache_known_read_tokens"), read);
                assert_eq!(
                    row.get::<Option<i64>, _>("cache_known_input_tokens"),
                    denominator
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
                        min_cost_micros: Some(financial_input + 3),
                        max_cost_micros: Some(financial_input + 3),
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
                    assert_eq!(cache.reported_read_tokens, cached);
                    assert_eq!(cache.reported_requests, i64::from(read.is_some()));
                    assert_eq!(cache.known_read_tokens, if eligible { cached } else { 0 });
                    assert_eq!(cache.known_input_tokens, denominator.unwrap_or(0));
                    assert_eq!(cache.eligible_requests, i64::from(eligible));
                    assert_eq!(cache.unknown_requests, i64::from(!eligible));
                    assert_eq!(cache.hit_rate, rate);
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
                            min_cost_micros: Some(financial_input + 4),
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
                        if eligible { 1 } else { 2 }
                    );
                    assert_eq!(
                        stats.summary.cache_usage.eligible_requests,
                        i64::from(read.is_some())
                    );
                    assert_eq!(stats.summary.cache_usage.hit_rate, rate);
                }
            }
        }
    }
}

async fn coverage_rebuild_contract(database: &Database, postgres: bool) {
    for deferred in [false, true] {
        let (key, reservation, request_id) = admitted(database, false).await;
        for statement in [
            "UPDATE request_records SET created_at = 1577836800000 WHERE id = $1",
            "UPDATE request_record_locators SET created_at = 1577836800000 WHERE id = $1",
        ] {
            sqlx::query(statement)
                .bind(request_id.to_string())
                .execute(&database.pool)
                .await
                .unwrap();
        }
        let mut input = finish(&key, &reservation, request_id);
        input.status_code = 500;
        input.error_code = Some("cache-coverage-fixture-error");
        input.usage = crate::api::parse_cache_usage_contract(
            &serde_json::json!({"usage":{"prompt_tokens":7,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":2}}}),
        );
        if deferred {
            database.finish_proxy_request_deferred(input).await.unwrap();
            let owner = Uuid::now_v7();
            database
                .claim_terminal_projection_tasks(owner, 32)
                .await
                .unwrap();
            assert!(
                database
                    .project_claimed_terminal_projection_task(owner, request_id)
                    .await
                    .unwrap()
            );
        } else {
            database.finish_proxy_request(input).await.unwrap();
        }
        sqlx::query("UPDATE request_stats_facts SET cost_micros = 11 WHERE request_id = $1")
            .bind(request_id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        for expected in [1, 0] {
            let report = database
                .backfill_failed_request_costs(FailedRequestCostBackfillInput {
                    apply: true,
                    batch_size: 10,
                    from_created_at: 1577836800000,
                    to_created_at: 1577923200000,
                    confirmed_legacy_null_request_ids: vec![],
                    after: None,
                })
                .await
                .unwrap();
            assert_eq!(report.changed_rows, expected);
        }
        let filter = StatsFilter {
            from_created_at: Some(1577836800000),
            to_created_at: Some(1577923199999),
            ..Default::default()
        };
        let assert_coverage = |stats: SelfStats| {
            assert_eq!(stats.summary.total_requests, 1);
            assert_eq!(stats.summary.cache_usage.reported_read_tokens, 2);
            assert_eq!(stats.summary.cache_usage.known_read_tokens, 2);
            assert_eq!(stats.summary.cache_usage.known_input_tokens, 7);
            assert_eq!(stats.summary.cache_usage.eligible_requests, 1);
            assert_eq!(stats.summary.cache_usage.unknown_requests, 0);
            assert_eq!(stats.summary.cache_usage.hit_rate, Some(2.0 / 7.0));
        };
        assert_coverage(
            database
                .stats_filtered(key.key_id, filter.clone())
                .await
                .unwrap(),
        );
        if postgres {
            if deferred {
                sqlx::query("DELETE FROM request_records WHERE id = $1")
                    .bind(request_id.to_string())
                    .execute(&database.pool)
                    .await
                    .unwrap();
            }
            sqlx::query("DELETE FROM request_stats_facts WHERE request_id = $1")
                .bind(request_id.to_string())
                .execute(&database.pool)
                .await
                .unwrap();
            let rebuild =
                include_str!("../../../../scripts/maintenance/reconcile-observability-day.sql")
                    .replace(":'day'", "'2020-01-01'");
            for _ in 0..2 {
                maintenance(database, rebuild.clone()).await.unwrap();
                assert_coverage(
                    database
                        .stats_filtered(key.key_id, filter.clone())
                        .await
                        .unwrap(),
                );
            }
        }
        assert_counts(database, request_id, reservation.id, 1).await;
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
    legacy_coverage_contract(&database).await;
    coverage_contract(&database).await;
    coverage_rebuild_contract(&database, false).await;
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
    legacy_coverage_contract(&database).await;
    coverage_contract(&database).await;
    coverage_rebuild_contract(&database, true).await;
    database.pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
}
