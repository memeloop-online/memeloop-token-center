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
    // Upgrade a durable first-slice frozen record that predates read_model.
    sqlx::query("UPDATE request_records SET duration_ms=21 WHERE id=$1")
        .bind(request_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    let current = native_pending(database).await;
    let mut legacy = serde_json::to_value(&current[0]).unwrap();
    legacy.as_object_mut().unwrap().remove("read_model");
    sqlx::query(
        "UPDATE request_history_export SET snapshot=$1 WHERE source='native' AND request_id=$2",
    )
    .bind(serde_json::to_string(&legacy).unwrap())
    .bind(request_id.to_string())
    .execute(&database.pool)
    .await
    .unwrap();
    // During mixed-queue rollout the already frozen immutable delivery wins.
    let modern_replay = native_pending(database).await;
    assert_eq!(
        serde_json::to_value(&modern_replay[0]).unwrap(),
        serde_json::to_value(&current[0]).unwrap()
    );
    // A pre-156 journal has no immutable queue, including sibling events.
    // Preserve every already frozen delivery in its legacy journal first;
    // otherwise event-only immutable priority correctly defers this native row.
    sqlx::query("UPDATE request_history_export SET snapshot=(SELECT i.delivery_snapshot FROM request_history_immutable_outbox i WHERE i.revision=request_history_export.revision) WHERE snapshot IS NULL AND EXISTS (SELECT 1 FROM request_history_immutable_outbox i WHERE i.revision=request_history_export.revision AND i.delivery_snapshot IS NOT NULL)")
        .execute(&database.pool).await.unwrap();
    sqlx::query("DELETE FROM request_history_immutable_outbox")
        .execute(&database.pool)
        .await
        .unwrap();
    let replay = native_pending(database).await;
    assert!(replay[0].read_model.is_null());
    let mut changed = replay[0].clone();
    changed.payload.as_mut().unwrap()["model"] = serde_json::json!("not-the-frozen-payload");
    database.history_acknowledge(&[changed]).await.unwrap();
    assert_eq!(native_pending(database).await.len(), 1);
    database.history_acknowledge(&replay).await.unwrap();
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
    assert_eq!(page.requests[0]["archive_state"], serde_json::json!("gap"));
    let stored = client.snapshots(&query).await.unwrap();
    assert_eq!(
        stored[0].read_model["archive"]["view"]["archive_state"],
        serde_json::json!("gap")
    );
    assert_eq!(
        stored[0].read_model["archive"]["request_archive_state"],
        serde_json::json!("pending")
    );
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
    // Report field paths only, never historical values or credentials.
    fn differing_paths(left: &Value, right: &Value, path: &str, output: &mut Vec<String>) {
        if left == right {
            return;
        }
        match (left, right) {
            (Value::Object(left), Value::Object(right)) => {
                let keys = left
                    .keys()
                    .chain(right.keys())
                    .collect::<std::collections::BTreeSet<_>>();
                for key in keys {
                    match (left.get(key), right.get(key)) {
                        (Some(left), Some(right)) => {
                            differing_paths(left, right, &format!("{path}/{key}"), output)
                        }
                        _ => output.push(format!("{path}/{key}")),
                    }
                }
            }
            _ => output.push(path.to_owned()),
        }
    }
    for actual in client.snapshots(&query).await.unwrap() {
        let expected = database
            .history_snapshot(actual.identity.clone(), actual.revision)
            .await
            .unwrap();
        let mut paths = Vec::new();
        differing_paths(
            &serde_json::to_value(expected).unwrap(),
            &serde_json::to_value(&actual).unwrap(),
            "",
            &mut paths,
        );
        assert!(
            paths.is_empty(),
            "same-revision canonical mismatch source={} revision={} paths={paths:?}",
            actual.identity.source,
            actual.revision
        );
    }
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
    let expected_trends = serde_json::to_value(
        database
            .operator_usage_analysis_trends("history-tenant", source_filter.clone())
            .await
            .unwrap(),
    )
    .unwrap();
    database.history_switch(true).await.unwrap();
    assert_eq!(
        serde_json::to_value(
            database
                .operator_usage_analysis_trends("history-tenant", source_filter.clone())
                .await
                .unwrap()
        )
        .unwrap(),
        expected_trends
    );
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
    // Both live relational reads and exported history must resolve a client
    // chat id without confusing it with the internal conversation cluster.
    for (field, value, expected) in [
        (
            crate::filter_ast::TypedFilterField::ClientSessionId,
            "history-explicit".to_owned(),
            1,
        ),
        (
            crate::filter_ast::TypedFilterField::ClientSessionId,
            cluster.to_string(),
            0,
        ),
        (
            crate::filter_ast::TypedFilterField::SessionId,
            cluster.to_string(),
            1,
        ),
        (
            crate::filter_ast::TypedFilterField::ClientSessionId,
            "other-thread".to_owned(),
            0,
        ),
    ] {
        let filter = RequestListFilter {
            limit: 100,
            key_id: Some(key.key_id),
            typed_ast: Some(crate::filter_ast::TypedFilterAst {
                conditions: vec![crate::filter_ast::TypedFilterCondition {
                    field,
                    operator: crate::filter_ast::TypedFilterOperator::Equals,
                    value: crate::filter_ast::TypedFilterValue::Text(value),
                    upper: None,
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let source = database.source_history_list(&filter).await.unwrap();
        let history_query = crate::request_history_api::query_from_request_filter(
            crate::request_history::HistoryScope::Key {
                tenant_id: key.tenant_id,
                key_id: key.key_id,
            },
            &filter,
        )
        .unwrap();
        let history = client.list(&history_query).await.unwrap();
        assert_eq!(source.len(), expected);
        assert_eq!(history.requests.len(), expected);
        if expected == 1 {
            assert_eq!(source[0].request_id, request_id);
            assert_eq!(history.requests[0]["request_id"], request_id.to_string());
        }
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
    sqlx::query("INSERT INTO session_generated_titles (tenant_id,key_id,session_id,title,state,next_attempt_at,updated_at) VALUES ($1,$2,$3,'History generated title','ready',1,1) ON CONFLICT(tenant_id,key_id,session_id) DO UPDATE SET title='History generated title',state='ready'")
        .bind(key.tenant_id.to_string()).bind(key.key_id.to_string()).bind(cluster.to_string()).execute(&database.pool).await.unwrap();
    let named = database
        .operator_recent_sessions(
            "history-tenant",
            crate::db::LogicalSessionListFilter {
                limit: 100,
                state: "all".into(),
                query: Some("History generated".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(named.len(), 1);
    assert_eq!(named[0].session_id, cluster.to_string());
    assert_eq!(
        named[0].session_name.as_deref(),
        Some("History generated title")
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
    sqlx::query("UPDATE request_stats_facts SET upstream_account_id=$1 WHERE request_id=$2")
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
    // This isolated fact correction intentionally bypasses production rollup
    // maintenance. Restore it before the whole-source/rollup verification.
    sqlx::query("UPDATE request_stats_facts SET cost_micros=17 WHERE request_id=$1")
        .bind(request_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    crate::request_history::migration::drain(&database, &client, 500)
        .await
        .unwrap();
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
    sqlx::query("UPDATE request_records SET duration_ms=21 WHERE id=$1")
        .bind(request_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    assert!(database.history_refresh().await.is_err());
    crate::request_history::migration::drain(&database, &client, 500)
        .await
        .unwrap();
    database.history_refresh().await.unwrap();
    while !database.history_backfill_done().await.unwrap() {
        database.history_backfill_page(1).await.unwrap();
        crate::request_history::migration::drain(&database, &client, 500)
            .await
            .unwrap();
    }
    crate::request_history::migration::verify(&database, &client)
        .await
        .unwrap();
    database.close().await;
    state.db.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
}

#[tokio::test]
async fn sqlite_immutable_history_late_conversation_revision_survives_stale_ack() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("late-history.db").display()
    ))
    .await
    .unwrap();
    database.migrate().await.unwrap();
    database.history_start().await.unwrap();
    let (key, request_id, _) = seed(&database).await;
    let initial = database.history_pending(500).await.unwrap();
    let initial_revision = initial
        .iter()
        .filter(|snapshot| snapshot.identity.source == "native")
        .map(|snapshot| snapshot.revision)
        .max()
        .unwrap();
    database.history_acknowledge(&initial).await.unwrap();
    assert_eq!(database.history_pending_count().await.unwrap(), 0);
    let cluster = database
        .record_conversation_observation(
            &key,
            request_id,
            &serde_json::json!({"messages":[{"role":"user","content":"fixture"}]}),
            &crate::conversation::ConversationHints {
                session_id: Some("late-immutable-session".into()),
                session_name: Some("late immutable name".into()),
                compaction: true,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let corrected = database.history_pending(500).await.unwrap();
    let native = corrected
        .iter()
        .find(|snapshot| snapshot.identity.source == "native")
        .unwrap();
    assert!(native.revision > initial_revision);
    assert_eq!(
        native.payload.as_ref().unwrap()["session_context"]["session_id"],
        cluster.to_string()
    );
    assert_eq!(native.payload.as_ref().unwrap()["compaction"], true);
    let corrected_hash = crate::request_history::canonical_hash(native).unwrap();
    database.history_acknowledge(&initial).await.unwrap();
    let replay = database.history_pending(500).await.unwrap();
    let native_replay = replay
        .iter()
        .find(|snapshot| snapshot.identity.source == "native")
        .unwrap();
    assert_eq!(
        crate::request_history::canonical_hash(native_replay).unwrap(),
        corrected_hash
    );
    database.history_acknowledge(&replay).await.unwrap();
    assert_eq!(database.history_pending_count().await.unwrap(), 0);
}

#[tokio::test]
async fn sqlite_immutable_request_history_projection_and_event_only_revisions() {
    use crate::db::requests::conversations::{
        ConversationProjectionEnqueueInput, enqueue_conversation_projection_in_transaction,
    };
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("projection.sqlite").display()
    ))
    .await
    .unwrap()
    .with_archive(crate::archive::ArchiveStore::memory_for_test());
    database.migrate().await.unwrap();
    database.history_start().await.unwrap();
    let (key, request_id, _) = seed(&database).await;
    finish(&database, request_id).await;
    let body = serde_json::json!({"messages":[{"role":"user","content":"event-fence fixture"}]});
    let hints = crate::conversation::ConversationHints {
        session_id: Some("immutable-projector-session".into()),
        ..Default::default()
    };
    let mut transaction = database.begin_write_transaction().await.unwrap();
    enqueue_conversation_projection_in_transaction(
        &mut transaction,
        ConversationProjectionEnqueueInput {
            request_id,
            key: &key,
            hints: &hints,
            client_name: None,
            upstream_response_id: None,
            observed_at: unix_millis(),
        },
    )
    .await
    .unwrap();
    transaction.commit().await.unwrap();
    let owner = Uuid::now_v7();
    assert_eq!(
        database
            .claim_conversation_projection_tasks(owner, 1)
            .await
            .unwrap()[0]
            .request_id,
        request_id
    );
    let atoms = crate::conversation::extract_atoms(&body);
    let nodes = crate::conversation::build_prefix(&atoms);
    assert!(
        database
            .prepare_session_body_archive(owner, request_id, atoms, &nodes, &[])
            .await
            .unwrap()
    );
    // The actual projector captures its observation and then emits the event
    // in the same transaction. Neither capture may conflict at the parent revision.
    assert!(
        database
            .project_claimed_conversation_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    let projected: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_events WHERE request_id=$1 AND event_kind='projected'",
    )
    .bind(request_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(projected, 1);
    let mut transaction = database.begin_write_transaction().await.unwrap();
    super::super::history_immutable_outbox::capture_request_in_transaction(
        &mut transaction,
        &request_id.to_string(),
    )
    .await
    .unwrap();
    let parent_revision: i64 = sqlx::query_scalar(
        "SELECT revision FROM request_history_export WHERE source='native' AND request_id=$1",
    )
    .bind(request_id.to_string())
    .fetch_one(&mut *transaction)
    .await
    .unwrap();
    let event_id = Uuid::now_v7();
    sqlx::query("INSERT INTO request_events (tenant_id,event_id,request_id,event_at,event_kind,key_id,protocol,model,status_code,duration_ms,input_tokens,output_tokens,cost_micros,error_code) SELECT tenant_id,$1,id,$2,'event_fence_fixture',key_id,protocol,model,202,duration_ms,input_tokens,output_tokens,cost_micros,error_code FROM request_records WHERE id=$3")
        .bind(event_id.to_string()).bind(unix_millis()).bind(request_id.to_string()).execute(&mut *transaction).await.unwrap();
    super::super::history_immutable_outbox::capture_request_in_transaction(
        &mut transaction,
        &request_id.to_string(),
    )
    .await
    .unwrap();
    let same_parent_revision: i64 = sqlx::query_scalar(
        "SELECT revision FROM request_history_export WHERE source='native' AND request_id=$1",
    )
    .bind(request_id.to_string())
    .fetch_one(&mut *transaction)
    .await
    .unwrap();
    assert_eq!(parent_revision, same_parent_revision);
    transaction.commit().await.unwrap();
    // Repeated journal capture must preserve the parent and each event's bytes.
    database.history_capture_pending(64).await.unwrap();
    database.history_capture_pending(64).await.unwrap();
    let raw: String = sqlx::query_scalar("SELECT snapshot FROM request_history_immutable_outbox WHERE source='event' AND request_id=$1")
        .bind(event_id.to_string()).fetch_one(&database.pool).await.unwrap();
    let envelope: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(envelope["originals"]["event"]["status_code"], 202);
    assert_eq!(
        envelope["originals"]["event"]["event_kind"],
        "event_fence_fixture"
    );
    assert_eq!(envelope["payload"]["status_code"], 200);
    let pending = database.history_pending(500).await.unwrap();
    let replay = database.history_pending(500).await.unwrap();
    assert_eq!(
        serde_json::to_value(&pending).unwrap(),
        serde_json::to_value(&replay).unwrap()
    );
    database.history_acknowledge(&pending).await.unwrap();
    assert_eq!(database.history_pending_count().await.unwrap(), 0);
}

async fn immutable_history_asset_revision_fence(database: &Database) {
    database.migrate().await.unwrap();
    database.history_start().await.unwrap();
    let (key, _, _) = seed(database).await;
    let first = Uuid::now_v7();
    let second = Uuid::now_v7();
    for job in [first, second] {
        sqlx::query("INSERT INTO generation_jobs (id,tenant_id,key_id,upstream_account_id,reservation_id,public_model,upstream_model,driver,status,request_object,estimated_units,next_attempt_at,created_at,updated_at) VALUES ($1,$2,$3,$4,$5,'asset-model','asset-model','http-json','queued','gap://fixture/generation',1,1,1,1)")
            .bind(job.to_string()).bind(key.tenant_id.to_string()).bind(key.key_id.to_string())
            .bind(Uuid::now_v7().to_string()).bind(Uuid::now_v7().to_string()).execute(&database.pool).await.unwrap();
    }
    database.history_capture_pending(64).await.unwrap();
    async fn revision(database: &Database, job: Uuid) -> i64 {
        sqlx::query_scalar("SELECT revision FROM request_history_export WHERE source='generation' AND request_id=$1")
            .bind(job.to_string()).fetch_one(&database.pool).await.unwrap()
    }
    async fn raw(database: &Database, revision: i64) -> String {
        sqlx::query_scalar(
            "SELECT snapshot FROM request_history_immutable_outbox WHERE revision=$1",
        )
        .bind(revision)
        .fetch_one(&database.pool)
        .await
        .unwrap()
    }
    let initial_first = revision(database, first).await;
    let initial_second = revision(database, second).await;
    let initial_raw = raw(database, initial_first).await;
    let ledger_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger_entries")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    let asset = Uuid::now_v7();
    sqlx::query("INSERT INTO generation_assets (id,job_id,asset_index,object_locator,mime_type,size_bytes,filename,created_at) VALUES ($1,$2,0,'gap://fixture/asset','image/png',0,'fixture.png',1)")
        .bind(asset.to_string()).bind(first.to_string()).execute(&database.pool).await.unwrap();
    database.history_capture_pending(64).await.unwrap();
    let inserted_first = revision(database, first).await;
    assert!(inserted_first > initial_first);
    assert_eq!(revision(database, second).await, initial_second);
    let inserted: Value = serde_json::from_str(&raw(database, inserted_first).await).unwrap();
    assert_eq!(inserted["originals"]["assets"].as_array().unwrap().len(), 1);
    // Moving a metadata row must invalidate both its OLD and NEW parent.
    sqlx::query("UPDATE generation_assets SET job_id=$1 WHERE id=$2")
        .bind(second.to_string())
        .bind(asset.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    database.history_capture_pending(64).await.unwrap();
    let moved_first = revision(database, first).await;
    let moved_second = revision(database, second).await;
    assert!(moved_first > inserted_first);
    assert!(moved_second > initial_second);
    let old_parent: Value = serde_json::from_str(&raw(database, moved_first).await).unwrap();
    let new_parent: Value = serde_json::from_str(&raw(database, moved_second).await).unwrap();
    assert!(
        old_parent["originals"]["assets"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        new_parent["originals"]["assets"].as_array().unwrap().len(),
        1
    );
    sqlx::query("DELETE FROM generation_assets WHERE id=$1")
        .bind(asset.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    database.history_capture_pending(64).await.unwrap();
    let deleted_second = revision(database, second).await;
    assert!(deleted_second > moved_second);
    let deleted: Value = serde_json::from_str(&raw(database, deleted_second).await).unwrap();
    assert!(
        deleted["originals"]["assets"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(raw(database, initial_first).await, initial_raw);
    let ledger_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger_entries")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(ledger_before, ledger_after);
}

#[tokio::test]
async fn sqlite_immutable_request_history_asset_insert_move_delete_revisions() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("assets.sqlite").display()
    ))
    .await
    .unwrap();
    immutable_history_asset_revision_fence(&database).await;
}

#[tokio::test]
#[ignore = "existing CI supplies the real PostgreSQL service"]
async fn request_history_real_services_acceptance_asset_insert_move_delete_revisions() {
    let postgres = std::env::var("MTC_TEST_POSTGRES_URL").expect("PostgreSQL acceptance URL");
    let admin = sqlx::PgPool::connect(&postgres).await.unwrap();
    let schema = format!("history_assets_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&postgres).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let database = Database::connect(isolated.as_str()).await.unwrap();
    immutable_history_asset_revision_fence(&database).await;
    database.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}

#[tokio::test]
#[ignore = "existing CI supplies the real PostgreSQL service"]
async fn request_history_real_services_acceptance_start_nowait_rolls_back_partial_locks() {
    let postgres = std::env::var("MTC_TEST_POSTGRES_URL").expect("PostgreSQL acceptance URL");
    let admin = sqlx::PgPool::connect(&postgres).await.unwrap();
    let schema = format!("history_start_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&postgres).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let database = Database::connect_with_max(isolated.as_str(), 2)
        .await
        .unwrap();
    database.migrate().await.unwrap();
    let mut writer = database.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE request_records IN ROW EXCLUSIVE MODE")
        .execute(&mut *writer)
        .await
        .unwrap();
    let error = tokio::time::timeout(std::time::Duration::from_secs(3), database.history_start())
        .await
        .expect("NOWAIT must not queue behind a business writer")
        .unwrap_err();
    let HistoryError::Sql(error) = error else {
        panic!("unexpected history start error");
    };
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("55P03")
    );
    assert_eq!(database.history_mode().await.unwrap(), "off");
    let installed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_proc p ON p.oid=t.tgfoid WHERE c.relnamespace=to_regnamespace(current_schema()) AND NOT t.tgisinternal AND p.proname LIKE 'history_%' AND position('request_history_export' in p.prosrc)>0")
        .fetch_one(&database.pool).await.unwrap();
    assert_eq!(installed, 0);
    let writer_alive: i32 = sqlx::query_scalar("SELECT 1")
        .fetch_one(&mut *writer)
        .await
        .unwrap();
    assert_eq!(
        writer_alive, 1,
        "maintenance must not abort the business transaction"
    );
    writer.rollback().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), database.history_start())
        .await
        .expect("failed installation must release every acquired table lock")
        .unwrap();
    assert_eq!(database.history_mode().await.unwrap(), "shadow");
    database.pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}

#[tokio::test]
#[ignore = "existing CI supplies real PostgreSQL and ClickHouse services"]
async fn request_history_real_services_acceptance_event_original_survives_ack_and_source_removal() {
    let postgres = std::env::var("MTC_TEST_POSTGRES_URL").expect("PostgreSQL acceptance URL");
    let endpoint = std::env::var("MTC_TEST_CLICKHOUSE_URL").expect("ClickHouse acceptance URL");
    let admin = sqlx::PgPool::connect(&postgres).await.unwrap();
    let schema = format!("history_fidelity_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&postgres).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let database = Database::connect(isolated.as_str())
        .await
        .unwrap()
        .with_archive(crate::archive::ArchiveStore::memory_for_test());
    database.migrate().await.unwrap();
    database.history_start().await.unwrap();
    let (key, request_id, _) = seed(&database).await;
    finish(&database, request_id).await;
    let event_id = Uuid::now_v7();
    sqlx::query("INSERT INTO request_events (tenant_id,event_id,request_id,event_at,event_kind,key_id,protocol,model,status_code,duration_ms,input_tokens,output_tokens,cost_micros,error_code) SELECT tenant_id,$1,id,$2,'original_fidelity_fixture',key_id,protocol,model,202,3,0,0,7,NULL FROM request_records WHERE id=$3")
        .bind(event_id.to_string()).bind(unix_millis()).bind(request_id.to_string()).execute(&database.pool).await.unwrap();
    let mut transaction = database.pool.begin().await.unwrap();
    let original = super::super::history_immutable_outbox::original_event_in_transaction(
        &mut transaction,
        event_id,
    )
    .await
    .unwrap();
    transaction.commit().await.unwrap();
    let pending = database.history_pending(500).await.unwrap();
    let snapshot = pending
        .iter()
        .find(|snapshot| {
            snapshot.identity.source == "event" && snapshot.identity.request_id == event_id
        })
        .unwrap()
        .clone();
    assert_eq!(snapshot.payload.as_ref().unwrap()["status_code"], 200);
    assert_eq!(
        crate::request_history::event_original::reconstruct(
            &snapshot.identity,
            snapshot.payload.as_ref().unwrap(),
            &snapshot.read_model
        )
        .unwrap(),
        original
    );
    let expected = database
        .history_snapshot(snapshot.identity.clone(), snapshot.revision)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&expected).unwrap(),
        serde_json::to_value(&snapshot).unwrap()
    );
    reqwest::Client::new()
        .post(&endpoint)
        .body(format!("CREATE DATABASE {schema}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let client = ClickHouse::new(
        database
            .http_clients
            .handle(crate::http_settings::HttpPurpose::General),
        &endpoint,
        schema.clone(),
        "default".into(),
        String::new(),
    )
    .unwrap();
    client.initialize().await.unwrap();
    client.append(&pending).await.unwrap();
    client.append(&pending).await.unwrap();
    database.history_acknowledge(&pending).await.unwrap();
    assert_eq!(database.history_pending_count().await.unwrap(), 0);
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_history_immutable_outbox WHERE request_id=$1",
    )
    .bind(event_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(remaining, 0);
    // Isolated fixture retirement: stop its capture before removing its source.
    // Production retirement remains gated on the complete inventory/proofs.
    sqlx::query("UPDATE request_history_state SET mode='off' WHERE id=1")
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM request_events WHERE event_id=$1")
        .bind(event_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    let rows = client
        .event_snapshots(
            crate::request_history::HistoryScope::Key {
                tenant_id: key.tenant_id,
                key_id: key.key_id,
            },
            0,
            None,
            500,
        )
        .await
        .unwrap();
    let copies: Vec<_> = rows
        .iter()
        .filter(|row| row.identity.request_id == event_id)
        .collect();
    assert_eq!(
        copies.len(),
        1,
        "duplicate deliveries have one logical latest event"
    );
    let restored = crate::request_history::event_original::reconstruct(
        &copies[0].identity,
        copies[0].payload.as_ref().unwrap(),
        &copies[0].read_model,
    )
    .unwrap();
    assert_eq!(restored, original);
    assert_eq!(restored["status_code"], 202);
    assert_eq!(restored["input_tokens"], 0);
    assert_eq!(restored["cost_micros"], 7);
    database.close().await;
    reqwest::Client::new()
        .post(&endpoint)
        .body(format!("DROP DATABASE {schema}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
