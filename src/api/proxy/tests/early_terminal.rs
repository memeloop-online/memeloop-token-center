use super::*;

mod journal_isolation;

async fn reject_without_candidates(fixture: &CodexRouteFixture) -> Response {
    router_for_role(fixture.state.clone(), RuntimeRole::Gateway)
        .oneshot(
            Request::post("/v1/responses")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {}", fixture.key))
                .body(Body::from(
                    json!({"model": fixture.model, "input": "secret-body-canary"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn empty_candidates_have_typed_503_list_stats_and_one_unbilled_terminal() {
    let fixture = codex_route_fixture("early-terminal-empty").await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE upstream_accounts SET status = 'disabled' WHERE id = $1")
        .bind(fixture.upstream_account_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let response = reject_without_candidates(&fixture).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()[header::RETRY_AFTER], "1");
    let request_id =
        Uuid::parse_str(response.headers()[REQUEST_ID_HEADER].to_str().unwrap()).unwrap();
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(body["error"]["code"], "no_authorized_upstream_candidate");
    fixture.state.early_requests.drain_for_test().await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].request_id, request_id);
    assert_eq!(rows[0].status_code, Some(503));
    assert_eq!(rows[0].model, fixture.model);
    assert_eq!(
        rows[0].error_code.as_deref(),
        Some("no_authorized_upstream_candidate")
    );
    assert_eq!(
        rows[0].terminal_cause_code.as_deref(),
        Some("gateway_pre_admission")
    );
    assert_eq!(
        rows[0].usage_basis,
        Some(crate::model::RequestUsageBasis::NotObserved)
    );
    assert!(!rows[0].billing.billable);
    assert!(rows[0].upstream_account_id.is_none());
    assert!(rows[0].route_id.is_none());
    let tenant_id: String = sqlx::query_scalar("SELECT tenant_id FROM key_records WHERE id = $1")
        .bind(fixture.key_id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    let terminal = crate::db::EarlyRequestTerminal {
        request_id,
        key_id: fixture.key_id,
        tenant_id: Uuid::parse_str(&tenant_id).unwrap(),
        created_at: rows[0].created_at,
        completed_at: rows[0].completed_at.unwrap(),
        duration_ms: rows[0].duration_ms.unwrap(),
        protocol: "openai".into(),
        model: fixture.model.clone(),
        status_code: 503,
        error_code: "no_authorized_upstream_candidate".into(),
    };
    let (first, second) = tokio::join!(
        fixture.state.db.record_early_request_terminal(&terminal),
        fixture.state.db.record_early_request_terminal(&terminal),
    );
    first.unwrap();
    second.unwrap();
    for (statement, expected) in [
        ("SELECT COUNT(*) FROM request_records", 1_i64),
        (
            "SELECT COUNT(*) FROM request_events WHERE event_kind = 'finished'",
            1,
        ),
        (
            "SELECT COUNT(*) FROM request_events WHERE event_kind = 'started'",
            0,
        ),
        (
            "SELECT COUNT(*) FROM request_stats_facts WHERE status_class = 'failure'",
            1,
        ),
        ("SELECT SUM(requests) FROM request_daily_aggregates", 1),
        ("SELECT SUM(requests) FROM usage_daily_aggregates", 1),
        ("SELECT COUNT(*) FROM usage_reservations", 0),
        (
            "SELECT COUNT(*) FROM ledger_entries WHERE kind = 'usage'",
            0,
        ),
        ("SELECT SUM(cost_micros) FROM request_records", 0),
    ] {
        let actual: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(statement.to_owned()))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(actual, expected, "{statement}");
    }
    pool.close().await;
}

#[tokio::test]
async fn early_terminal_database_lock_does_not_hold_failure_response_and_replays() {
    let fixture = codex_route_fixture("early-terminal-lock").await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE upstream_accounts SET status = 'disabled' WHERE id = $1")
        .bind(fixture.upstream_account_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let mut blocker = pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    let response =
        tokio::time::timeout(Duration::from_secs(2), reject_without_candidates(&fixture))
            .await
            .expect("failure response must not wait for a database writer");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(!response.headers().contains_key("x-mtc-request-recording"));
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_records")
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    assert_eq!(count, 0);
    blocker.rollback().await.unwrap();
    fixture.state.early_requests.drain_for_test().await;
    assert_eq!(
        fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    pool.close().await;
}

#[tokio::test]
async fn authenticated_body_admission_failure_has_no_supplier_or_reservation() {
    let fixture = codex_route_fixture("early-terminal-body").await;
    let memory = fixture.state.proxy_memory_budget.reservation();
    assert!(memory.try_grow(fixture.state.config.proxy_memory_budget_bytes as usize, 1));
    let response = reject_without_candidates(&fixture).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    drop(memory);
    fixture.state.early_requests.drain_for_test().await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].model.is_empty());
    assert_eq!(rows[0].error_code.as_deref(), Some("service_overloaded"));
    assert!(!rows[0].billing.billable);
    assert!(rows[0].upstream_account_id.is_none());
}

#[tokio::test]
async fn admitted_upstream_failure_keeps_its_original_terminal() {
    let fixture = codex_route_fixture("early-terminal-upstream").await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(json!({"error":{"message":"invalid input"}})),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input":"synthetic"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    cancellation::assert_single_unknown_receipt(&fixture, 400, Some("http_400")).await;
    fixture.state.early_requests.drain_for_test().await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].upstream_account_id,
        Some(fixture.upstream_account_id)
    );
    assert_ne!(
        rows[0].terminal_cause_code.as_deref(),
        Some("gateway_pre_admission")
    );
    upstream.verify().await;
}

#[tokio::test]
async fn journal_recovery_projects_terminal_after_writer_replacement() {
    let fixture = codex_route_fixture("early-terminal-recovery").await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let tenant_id: String = sqlx::query_scalar("SELECT tenant_id FROM key_records WHERE id = $1")
        .bind(fixture.key_id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    let now = crate::db::unix_millis();
    let terminal = crate::db::EarlyRequestTerminal {
        request_id: Uuid::now_v7(),
        tenant_id: Uuid::parse_str(&tenant_id).unwrap(),
        key_id: fixture.key_id,
        created_at: now,
        completed_at: now,
        duration_ms: 0,
        model: fixture.model.clone(),
        protocol: "openai".into(),
        status_code: 503,
        error_code: "no_authorized_upstream_candidate".into(),
    };
    let directory = tempfile::tempdir().unwrap();
    let journal = crate::early_request::Journal::new(
        directory.path().to_owned(),
        fixture.state.db.independent_persistence_pool(),
    );
    assert!(journal.append(terminal.clone()).await);
    assert!(
        tokio::fs::read_dir(directory.path())
            .await
            .unwrap()
            .next_entry()
            .await
            .unwrap()
            .is_none(),
        "early terminals are recorded in the database, not a local spool"
    );
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].request_id, terminal.request_id);
    assert!(!rows[0].billing.billable);
    fixture
        .state
        .db
        .record_early_request_terminal(&terminal)
        .await
        .unwrap();
    let events: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_events WHERE request_id = $1")
            .bind(terminal.request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(events, 1);
    pool.close().await;
}
