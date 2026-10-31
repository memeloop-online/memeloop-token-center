use super::*;
use crate::request_history::{ClickHouse, HistoryQuery};

async fn seed(database: &Database) -> (AuthenticatedKey, Uuid, String) {
    let pepper = b"test-pepper-must-have-at-least-32-bytes";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "history-tenant".into(),
                principal_external_id: "history-member".into(),
                alias: "history-key".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ZERO,
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
    let request_id = Uuid::now_v7();
    database
        .record_request_started(NewRequest {
            request_id,
            key_id: key.key_id,
            tenant_id: key.tenant_id,
            protocol: "openai".into(),
            model: "history-model".into(),
            request_object: "gap://fixture/request".into(),
            reservation_id: Uuid::now_v7(),
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    (key, request_id, issued.key)
}

async fn finish(database: &Database, request_id: Uuid) {
    database
        .record_request_finished(FinishRequest {
            request_id,
            first_output_ms: None,
            generation_duration_ms: None,
            status_code: 200,
            duration_ms: 20,
            input_tokens: 10,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 5,
            service_tier: None,
            cost_micros: 17,
            error_code: None,
            response_object: "gap://fixture/response".into(),
        })
        .await
        .unwrap();
}

async fn native_pending(database: &Database) -> Vec<HistorySnapshot> {
    database
        .history_pending(500)
        .await
        .unwrap()
        .into_iter()
        .filter(|snapshot| snapshot.identity.source == "native")
        .collect()
}

async fn capture_replay(database: &Database) {
    database.migrate().await.unwrap();
    database.history_start().await.unwrap();
    let (_, request_id, _) = seed(database).await;
    let pending = native_pending(database).await;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].statistics.as_ref().unwrap().status, "pending");
    finish(database, request_id).await;
    database.history_acknowledge(&pending).await.unwrap();
    let corrected = native_pending(database).await;
    assert_eq!(corrected.len(), 1);
    assert!(corrected[0].revision > pending[0].revision);
    assert_eq!(
        corrected[0].statistics.as_ref().unwrap().cost_micros,
        Some(17)
    );
    let replay = native_pending(database).await;
    assert_eq!(
        serde_json::to_value(&corrected).unwrap(),
        serde_json::to_value(&replay).unwrap()
    );
    assert!(database.history_switch(true).await.is_err());
    database.history_acknowledge(&corrected).await.unwrap();
    assert!(native_pending(database).await.is_empty());
}

#[tokio::test]
async fn sqlite_request_history_capture_freezes_retries_and_preserves_newer_corrections() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("history.db").display()
    ))
    .await
    .unwrap();
    capture_replay(&database).await;
}

#[tokio::test]
async fn sqlite_request_history_failed_sink_retains_frozen_replay_across_restart() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("failed-export.db").display()
    );
    let database = Database::connect(&url).await.unwrap();
    database.migrate().await.unwrap();
    database.history_start().await.unwrap();
    seed(&database).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let service = axum::Router::new().route(
        "/",
        axum::routing::post(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, service).await.unwrap();
    });
    let client = ClickHouse::new(
        database
            .http_clients
            .handle(crate::http_settings::HttpPurpose::General),
        &endpoint,
        "default".into(),
        "default".into(),
        String::new(),
    )
    .unwrap();
    assert!(
        crate::request_history::migration::export_page(&database, &client, 1)
            .await
            .is_err()
    );
    let frozen = database.history_pending(1).await.unwrap();
    assert_eq!(frozen.len(), 1);
    let mut conflict = frozen[0].clone();
    conflict.payload.as_mut().unwrap()["model"] = serde_json::json!("conflicting-revision");
    assert!(matches!(
        client.append(&[frozen[0].clone(), conflict]).await,
        Err(HistoryError::Mismatch)
    ));
    database.close().await;
    let restarted = Database::connect(&url).await.unwrap();
    assert_eq!(
        serde_json::to_value(restarted.history_pending(1).await.unwrap()).unwrap(),
        serde_json::to_value(frozen).unwrap()
    );
    server.abort();
}

#[tokio::test]
#[ignore = "existing CI supplies real PostgreSQL and ClickHouse services"]
async fn request_history_real_services_acceptance() {
    let postgres = std::env::var("MTC_TEST_POSTGRES_URL").expect("PostgreSQL acceptance URL");
    let endpoint = std::env::var("MTC_TEST_CLICKHOUSE_URL").expect("ClickHouse acceptance URL");
    let admin = sqlx::PgPool::connect(&postgres).await.unwrap();
    let schema = format!("history_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&postgres).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let database = Database::connect(isolated.as_str()).await.unwrap();
    database.migrate().await.unwrap();
    database.history_start().await.unwrap();
    let (key, request_id, credential) = seed(&database).await;
    let state = crate::AppState::initialize(crate::config::Config::for_test(isolated.to_string()))
        .await
        .unwrap();
    let client = ClickHouse::new(
        database
            .http_clients
            .handle(crate::http_settings::HttpPurpose::General),
        &endpoint,
        "default".into(),
        "default".into(),
        String::new(),
    )
    .unwrap();
    client.initialize().await.unwrap();
    let old = native_pending(&database).await;
    client.append(&old).await.unwrap();
    finish(&database, request_id).await;
    let latest = native_pending(&database).await;
    client.append(&latest).await.unwrap();
    client.append(&old).await.unwrap();
    client.append(&latest).await.unwrap();
    database.history_acknowledge(&old).await.unwrap();
    assert_eq!(native_pending(&database).await.len(), 1);
    let query = HistoryQuery {
        scope: HistoryScope::Key {
            tenant_id: key.tenant_id,
            key_id: key.key_id,
        },
        from_created_at: None,
        to_created_at: None,
        cursor: None,
        limit: 1,
        predicates: Vec::new(),
    };
    let page = client.list(&query).await.unwrap();
    assert_eq!(page.requests.len(), 1);
    assert!(page.requests[0]["credential_identity"].is_null());
    let stats = client.statistics(&query).await.unwrap();
    assert_eq!(stats.summary.total_requests, 1);
    assert_eq!(stats.summary.successful_requests, 1);
    assert_eq!(stats.summary.input_tokens, 10);
    assert_eq!(stats.summary.total_cost.as_deref(), Some("0.000017"));
    let other = HistoryQuery {
        scope: HistoryScope::Key {
            tenant_id: Uuid::new_v4(),
            key_id: key.key_id,
        },
        ..query.clone()
    };
    assert!(client.list(&other).await.unwrap().requests.is_empty());
    let empty = HistoryQuery {
        scope: HistoryScope::EmptyTenant,
        ..query.clone()
    };
    assert!(client.list(&empty).await.unwrap().requests.is_empty());
    let ledger_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger_entries")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    database.history_acknowledge(&latest).await.unwrap();
    while !database.history_backfill_done().await.unwrap() {
        database.history_backfill_page(1).await.unwrap();
    }
    crate::request_history::migration::drain(&database, &client, 1)
        .await
        .unwrap();
    let report = crate::request_history::migration::verify(&database, &client)
        .await
        .unwrap();
    assert_eq!(report["count"], 1);
    database.history_switch(true).await.unwrap();
    database.history_switch(false).await.unwrap();
    for _ in 0..2 {
        sqlx::query("INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, input_tokens, output_tokens, cost_micros, request_object, reservation_id, currency) VALUES ($1, $2, $3, 1, 'openai', 'history-model', 0, 0, 0, 'gap://fixture/request', $4, 'USD')")
            .bind(Uuid::now_v7().to_string()).bind(key.tenant_id.to_string()).bind(key.key_id.to_string()).bind(Uuid::now_v7().to_string())
            .execute(&database.pool).await.unwrap();
    }
    sqlx::query("INSERT INTO request_record_locators (id, created_at, tenant_id, key_id) SELECT id, created_at, tenant_id, key_id FROM request_records WHERE created_at = 1")
        .execute(&database.pool).await.unwrap();
    crate::request_history::migration::drain(&database, &client, 1)
        .await
        .unwrap();
    let mut pagination = query.clone();
    let mut observed = std::collections::BTreeSet::new();
    loop {
        let page = client.list(&pagination).await.unwrap();
        for request in page.requests {
            assert!(observed.insert(request["request_id"].as_str().unwrap().to_owned()));
        }
        let Some(cursor) = page.next_cursor else {
            break;
        };
        pagination.cursor = Some(cursor);
    }
    assert_eq!(observed.len(), 3);
    let report = crate::request_history::migration::verify(&database, &client)
        .await
        .unwrap();
    assert_eq!(report["count"], 3);
    let stats = client.statistics(&query).await.unwrap();
    assert_eq!(stats.summary.pending_requests, 2);
    assert!(stats.summary.total_cost.is_none());
    let usage_filter = crate::db::UsageAnalysisFilter {
        from_created_at: Some(0),
        to_created_at: Some(i64::MAX),
        ..Default::default()
    };
    let usage_query = crate::request_history::HistoryQuery {
        limit: 500,
        ..query.clone()
    };
    let direct_usage = client
        .usage(
            HistoryScope::Tenant(key.tenant_id),
            &usage_filter,
            crate::db::UsageAnalysisProjection::Full,
            0,
            i64::MAX,
            "day",
        )
        .await
        .unwrap();
    let direct_monitoring = client.monitoring(usage_query).await.unwrap();
    assert_eq!(direct_usage.summary.requests, 1);
    assert_eq!(direct_monitoring.summary.requests, 1);
    let events = client
        .events(HistoryScope::Tenant(key.tenant_id), 0, None, 500)
        .await
        .unwrap();
    assert!(!events.is_empty());
    let latest_request = database
        .source_history_list(&RequestListFilter {
            limit: 100,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_iter()
        .find(|view| view.request_id == request_id)
        .unwrap();
    for event in events.iter().filter(|event| event.request_id == request_id) {
        assert_eq!(event.lifecycle_state, latest_request.lifecycle_state);
        assert_eq!(event.usage, latest_request.usage);
        assert_eq!(event.compaction, latest_request.compaction);
    }
    let source_to = unix_millis();
    let source_filter = crate::db::UsageAnalysisFilter {
        from_created_at: Some(source_to - 86_400_000),
        to_created_at: Some(source_to),
        ..Default::default()
    };
    let mut expected_projections = Vec::new();
    for projection in [
        crate::db::UsageAnalysisProjection::Full,
        crate::db::UsageAnalysisProjection::Overview,
        crate::db::UsageAnalysisProjection::Trends,
        crate::db::UsageAnalysisProjection::Dimensions,
        crate::db::UsageAnalysisProjection::Sessions,
        crate::db::UsageAnalysisProjection::Heatmap,
        crate::db::UsageAnalysisProjection::OutputRate,
    ] {
        expected_projections.push((
            projection,
            serde_json::to_value(
                database
                    .projected_usage_analysis(
                        Some("history-tenant"),
                        source_filter.clone(),
                        projection,
                    )
                    .await
                    .unwrap(),
            )
            .unwrap(),
        ));
    }
    database.history_switch(true).await.unwrap();
    let source = database
        .source_history_list(&RequestListFilter {
            limit: 100,
            ..Default::default()
        })
        .await
        .unwrap();
    let active = database
        .list_requests_filtered(
            key.key_id,
            RequestListFilter {
                limit: 100,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(source.len(), active.len());
    let refs = database
        .request_archive_refs(key.key_id, request_id)
        .await
        .unwrap();
    let view = active
        .iter()
        .find(|view| view.request_id == request_id)
        .unwrap();
    assert_eq!(
        serde_json::to_value(&refs.view).unwrap(),
        serde_json::to_value(view).unwrap()
    );
    assert!(
        database
            .history_archive_refs(&client, HistoryScope::EmptyTenant, request_id)
            .await
            .is_err()
    );
    assert!(
        database
            .request_archive_refs(Uuid::new_v4(), request_id)
            .await
            .is_err()
    );
    let usage = database
        .projected_usage_analysis(
            Some("history-tenant"),
            usage_filter,
            crate::db::UsageAnalysisProjection::Full,
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&usage).unwrap(),
        serde_json::to_value(&direct_usage).unwrap()
    );
    let monitoring = database
        .operator_monitoring_snapshot(
            crate::db::MonitoringScope::Tenant("history-tenant".into()),
            crate::db::MonitoringSnapshotFilter {
                from_created_at: 0,
                to_created_at: i64::MAX,
            },
        )
        .await
        .unwrap();
    assert_eq!(monitoring.summary.requests, 1);
    let active_events = database
        .request_events_after("history-tenant", 0, None, 500)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&active_events).unwrap(),
        serde_json::to_value(&events).unwrap()
    );
    assert!(
        database
            .request_events_after("missing-tenant", 0, None, 500)
            .await
            .unwrap()
            .is_empty()
    );
    let sessions = database
        .operator_recent_sessions(
            "history-tenant",
            crate::db::LogicalSessionListFilter {
                limit: 100,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].active_requests, 2);
    assert_eq!(sessions[0].requests, 1);
    let detail = database
        .logical_session_detail(
            key.tenant_id,
            key.key_id,
            &format!("unlinked:{}", key.key_id),
            crate::db::ConversationDetailFilter {
                limit: 100,
                before_created_at: None,
                before_request_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(detail.requests.len(), 3);
    for (projection, expected) in expected_projections {
        let actual = database
            .projected_usage_analysis(Some("history-tenant"), source_filter.clone(), projection)
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            expected,
            "{projection:?}"
        );
    }
    use tower::ServiceExt;
    let app = crate::api::router(state.clone());
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/self/v1/requests")
                .header("authorization", format!("Bearer {credential}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let response: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(response.as_array().unwrap().len(), 3);
    assert!(
        response
            .as_array()
            .unwrap()
            .iter()
            .all(|request| request["credential_identity"].is_null())
    );
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri(format!(
                    "/self/v1/stats?from_created_at=0&to_created_at={}",
                    i64::MAX
                ))
                .header("authorization", format!("Bearer {credential}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let response: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(response["summary"]["total_requests"], 3);
    assert_eq!(response["summary"]["pending_requests"], 2);
    let cluster = database
        .record_conversation_observation(
            &key,
            request_id,
            &serde_json::json!({"messages":[{"role":"user","content":"fixture"}]}),
            &crate::conversation::ConversationHints {
                session_id: Some("history-explicit".into()),
                compaction: true,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    crate::request_history::migration::drain(&database, &client, 500)
        .await
        .unwrap();
    for (association, count) in [("unlinked", 2), ("confirmed", 1)] {
        let mut filtered = query.clone();
        filtered.limit = 100;
        filtered
            .predicates
            .push(crate::request_history::HistoryPredicate {
                dimension: crate::request_history::Dimension::SessionAssociation,
                comparison: crate::request_history::Comparison::Equals,
                value: crate::request_history::PredicateValue::Text(association.into()),
            });
        assert_eq!(client.list(&filtered).await.unwrap().requests.len(), count);
    }
    let linked = database
        .request_archive_refs(key.key_id, request_id)
        .await
        .unwrap();
    assert_eq!(linked.view.compaction, Some(true));
    assert_eq!(
        linked
            .view
            .session_context
            .as_ref()
            .unwrap()
            .session_id
            .as_deref(),
        Some(cluster.to_string().as_str())
    );
    let mut accounts = Vec::new();
    for name in ["history-traffic", "history-zero"] {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO upstream_accounts (id,tenant_id,name,driver,auth_kind,config_json,status,credential_generation,created_at,updated_at) VALUES ($1,$2,$3,'http-json','api_key','{}','active',1,1,1)")
            .bind(id.to_string()).bind(key.tenant_id.to_string()).bind(name).execute(&database.pool).await.unwrap();
        accounts.push(id);
    }
    sqlx::query("UPDATE request_records SET upstream_account_id=$1 WHERE id=$2")
        .bind(accounts[0].to_string())
        .bind(request_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    crate::request_history::migration::drain(&database, &client, 500)
        .await
        .unwrap();
    let availability = database
        .upstream_account_availability(
            "history-tenant",
            crate::db::UpstreamAccountAvailabilityFilter {
                from_created_at: 0,
                to_created_at: i64::MAX,
            },
        )
        .await
        .unwrap();
    assert_eq!(availability.accounts.len(), 2);
    let traffic = availability
        .accounts
        .iter()
        .find(|a| a.upstream_account_id == accounts[0].to_string())
        .unwrap();
    assert_eq!(traffic.metrics.requests, 1);
    assert_eq!(traffic.terminal_outcomes.len(), 1);
    assert_eq!(traffic.terminal_outcomes[0].status, "success");
    assert_eq!(
        availability
            .accounts
            .iter()
            .find(|a| a.upstream_account_id == accounts[1].to_string())
            .unwrap()
            .metrics
            .requests,
        0
    );
    for (zone, offset) in [("America/New_York", None), ("UTC", Some(-480))] {
        let analysis = client
            .session_task_activity(
                key.tenant_id,
                &[(key.key_id, cluster), (key.key_id, cluster)],
                zone,
                offset,
            )
            .await
            .unwrap();
        assert_eq!(analysis["totals"]["requests"], 1);
        assert_eq!(analysis["totals"]["output_tokens"], 5);
        assert_eq!(analysis["active_days"], 1);
    }
    let empty = client
        .session_task_activity(key.tenant_id, &[], "UTC", None)
        .await
        .unwrap();
    assert_eq!(empty["totals"]["requests"], 0);
    sqlx::query("UPDATE request_stats_facts SET cost_micros=0 WHERE request_id=$1")
        .bind(request_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    crate::request_history::migration::drain(&database, &client, 500)
        .await
        .unwrap();
    let mut cost_query = query.clone();
    cost_query
        .predicates
        .push(crate::request_history::HistoryPredicate {
            dimension: crate::request_history::Dimension::CostMicros,
            comparison: crate::request_history::Comparison::GreaterThan,
            value: crate::request_history::PredicateValue::Integer(0),
        });
    assert_eq!(
        client
            .statistics(&cost_query)
            .await
            .unwrap()
            .summary
            .total_requests,
        0
    );
    assert_eq!(client.list(&cost_query).await.unwrap().requests.len(), 1);
    let pricing = database
        .pricing_model_usage(
            Some("history-tenant"),
            crate::db::StatsFilter {
                from_created_at: Some(0),
                to_created_at: Some(i64::MAX),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(pricing, vec![("history-model".to_owned(), 1, 10, 5)]);
    crate::request_history::migration::verify(&database, &client)
        .await
        .unwrap();
    database
        .history_save_verification(
            database.history_revision().await.unwrap(),
            &serde_json::json!({"read_model_version":1}),
        )
        .await
        .unwrap();
    assert!(database.history_switch(true).await.is_err());
    crate::request_history::migration::verify(&database, &client)
        .await
        .unwrap();
    database.history_switch(false).await.unwrap();
    let ledger_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger_entries")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(ledger_before, ledger_after);
    database.close().await;
    state.db.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
}
