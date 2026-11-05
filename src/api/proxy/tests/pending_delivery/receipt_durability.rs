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

async fn block_projection(fixture: &CodexRouteFixture) {
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("CREATE TRIGGER fail_receipt_projection BEFORE INSERT ON conversation_projection_outbox BEGIN SELECT RAISE(ABORT, 'RECEIPT_PROJECTION_FAULT'); END")
        .execute(&pool).await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn finance_completes_while_receipt_publication_and_conversation_projection_are_blocked() {
    let fixture = codex_route_fixture("receipt-before-preparation").await;
    let publication = fixture
        .state
        .pending_terminals
        .hold_publication_for_test()
        .await;
    let held = fixture
        .state
        .pending_terminals
        .hold_preparation_for_test()
        .await;
    block_projection(&fixture).await;
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
    let (receipt_id, receipt) = wait_for_receipt(&fixture).await;
    assert_eq!(receipt["conversation_state"], "Pending");
    assert_eq!(receipt["conversation"]["request_json"], json!({}));
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let financial: (String, i64, i64) = sqlx::query_as(
        "SELECT p.state, (SELECT COUNT(*) FROM ledger_entries WHERE source = p.reservation_id), \
         (SELECT COUNT(*) FROM terminal_projection_outbox WHERE request_id = p.request_id) \
         FROM pending_proxy_settlements p WHERE p.request_id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(financial, ("applied".into(), 1, 1));
    assert!(
        !fixture
            .state
            .pending_terminals
            .contains_receipt_for_test(receipt_id),
        "DB settlement must not falsely acknowledge the blocked journal publication"
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_projection_outbox WHERE request_id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 0, "held preparation has not enqueued its metadata");
    sqlx::query("DROP TRIGGER fail_receipt_projection")
        .execute(&pool)
        .await
        .unwrap();
    drop(held);
    drop(publication);
    assert_exactly_once_side_effects(&fixture, request_id, Some("resp-codex")).await;
    pool.close().await;
    upstream.verify().await;
}

#[tokio::test]
async fn another_pod_recovers_finance_and_conversation_seed_without_the_original_future() {
    let mut fixture = codex_route_fixture("receipt-remote-recovery").await;
    let held = fixture
        .state
        .pending_terminals
        .hold_preparation_for_test()
        .await;
    install_terminal_fault(&fixture).await;
    block_projection(&fixture).await;
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
    let persisted: Value = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(bytes) = fixture.state.archive.get(&location).await {
                break serde_json::from_slice(&bytes).unwrap();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("financial receipt must be remotely durable before losing the Pod");
    assert_eq!(persisted["conversation_state"], "Pending");
    assert_eq!(persisted["usage"], receipt["usage"]);
    assert_eq!(persisted["conversation"]["request_json"], json!({}));
    fixture.state.pending_terminals.stop_for_test().await;
    drop(held);
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("DROP TRIGGER fail_pending_terminal")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TRIGGER fail_receipt_projection")
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
    let request_uuid = Uuid::parse_str(request_id).unwrap();
    let source = owned_enqueued_source(
        &fixture,
        &pool,
        request_uuid,
        &json!({"model": fixture.model, "input": "retain source independently", "stream": false}),
    )
    .await;
    assert_eq!(source["input"], "retain source independently");
    assert_exactly_once_side_effects(&fixture, request_uuid, Some("resp-codex")).await;
    fixture.state.pending_terminals.stop_for_test().await;
    pool.close().await;
    upstream.verify().await;
}
