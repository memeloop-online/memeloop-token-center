use super::*;

async fn deliver(fixture: &CodexRouteFixture, upstream: &MockServer) {
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("receipt independent of conversation"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(upstream)
        .await;
    let response = send_codex_route(
        fixture,
        upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "retain source independently", "stream": false}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("receipt independent"));
}

#[tokio::test]
async fn finance_completes_while_conversation_preparation_is_blocked() {
    let fixture = codex_route_fixture("receipt-before-preparation").await;
    let held = fixture
        .state
        .pending_terminals
        .hold_preparation_for_test()
        .await;
    let upstream = MockServer::start().await;
    deliver(&fixture, &upstream).await;
    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    let request_id = rows[0].request_id;
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (3, 2));
    let (_, receipt) = wait_for_receipt(&fixture).await;
    assert_eq!(receipt["conversation_state"], "Pending");
    assert!(receipt["conversation"].is_null());
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_projection_outbox WHERE request_id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 0, "held preparation has not enqueued its metadata");
    drop(held);
    assert_exactly_once_side_effects(&fixture, request_id, Some("resp-codex")).await;
    pool.close().await;
    upstream.verify().await;
}

#[tokio::test]
async fn another_pod_recovers_finance_without_claiming_pending_conversation_is_done() {
    let mut fixture = codex_route_fixture("receipt-remote-recovery").await;
    let held = fixture
        .state
        .pending_terminals
        .hold_preparation_for_test()
        .await;
    install_terminal_fault(&fixture).await;
    let upstream = MockServer::start().await;
    deliver(&fixture, &upstream).await;
    let (receipt_id, receipt) = wait_for_receipt(&fixture).await;
    let namespace = fixture
        .state
        .pending_terminals
        .directory_for_test()
        .file_name()
        .unwrap()
        .to_owned();
    let location = format!(
        "terminal-receipts/v1/{}/{}.json",
        namespace.to_string_lossy(),
        receipt_id
    );
    let persisted: Value =
        serde_json::from_slice(&fixture.state.archive.get(&location).await.unwrap()).unwrap();
    assert_eq!(persisted["conversation_state"], "Pending");
    assert_eq!(persisted["usage"], receipt["usage"]);
    assert!(persisted["conversation"].is_null());
    fixture.state.pending_terminals.stop_for_test().await;
    drop(held);
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("DROP TRIGGER fail_pending_terminal")
        .execute(&pool)
        .await
        .unwrap();
    let second_directory = fixture._directory.path().join("second-pod").join(namespace);
    assert!(!second_directory.exists());
    fixture.state.pending_terminals =
        crate::proxy_lifecycle::pending_terminal::Journal::with_archive(
            second_directory,
            fixture.state.db.independent_persistence_pool(),
            fixture.state.archive.clone(),
        );
    wait_for_request_settlement(&fixture, 1).await;
    let request_id = receipt["request_id"].as_str().unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger_entries WHERE source = $1")
        .bind(receipt["reservation"]["id"].as_str().unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let state: String =
        sqlx::query_scalar("SELECT state FROM pending_proxy_settlements WHERE request_id = $1")
            .bind(request_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "applied");
    let still_pending: Value =
        serde_json::from_slice(&fixture.state.archive.get(&location).await.unwrap()).unwrap();
    assert_eq!(still_pending["conversation_state"], "Pending");
    assert!(still_pending["conversation"].is_null());
    fixture.state.pending_terminals.stop_for_test().await;
    pool.close().await;
    upstream.verify().await;
}
