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
    let request_id = receipt["request_id"].as_str().unwrap();
    let source_location =
        crate::gateway_body::request_spool::request_body_object_key(request_id).unwrap();
    let source_bytes = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(bytes) = fixture.state.archive.get(&source_location).await {
                break bytes;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // Model a source that is temporarily unavailable to the recovering Pod.
    // Only this isolated fixture's object is removed and restored.
    fixture
        .state
        .archive
        .delete(&source_location)
        .await
        .unwrap();
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
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM conversation_projection_outbox WHERE request_id = $1",
            )
            .bind(request_id)
            .fetch_one(&pool)
            .await
            .unwrap();
            if count == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("bounded identity must enqueue before the source HEAD succeeds");
    let retained = fixture.state.archive.get(&location).await.unwrap();
    let retained: Value = serde_json::from_slice(&retained).unwrap();
    assert_eq!(retained["conversation_state"], "Pending");
    assert_eq!(retained["usage"], persisted["usage"]);
    assert!(
        !fixture
            .state
            .archive
            .source_object_exists(&source_location)
            .await
            .unwrap()
    );
    fixture
        .state
        .archive
        .put(&source_location, source_bytes)
        .await
        .unwrap();
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

#[tokio::test]
#[ignore = "requires the CI PostgreSQL service"]
async fn postgres_slow_supplier_summary_preserves_terminal_and_recoverable_explanation() {
    let url = std::env::var("MTC_TEST_POSTGRES_URL").expect("CI PostgreSQL required");
    let admin = sqlx::PgPool::connect(&url).await.unwrap();
    let schema = format!("summary_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let mut fixture = codex_route_fixture_with_database(
        "summary-independent",
        "archive",
        Some(isolated.to_string()),
    )
    .await;
    fixture.state.pending_terminals.stop_for_test().await;
    let s3 = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(503).set_delay(Duration::from_secs(30)))
        .mount(&s3)
        .await;
    let mut config = Config::for_test(fixture.database_url.clone());
    config.archive_backend = ArchiveBackend::S3;
    config.s3_bucket = Some("summary-contract".into());
    config.s3_endpoint = Some(s3.uri());
    config.s3_access_key = Some("test-access".into());
    config.s3_secret_key = Some("test-secret".into());
    config.s3_allow_http = true;
    let summary_archive = crate::archive::ArchiveStore::from_config(&config)
        .await
        .unwrap();
    fixture.state.db = fixture
        .state
        .db
        .clone()
        .with_archive(summary_archive.clone());
    let directory = fixture
        .state
        .pending_terminals
        .directory_for_test()
        .to_owned();
    fixture.state.pending_terminals =
        crate::proxy_lifecycle::pending_terminal::Journal::with_archive(
            directory.clone(),
            fixture.state.db.independent_persistence_pool(),
            fixture.state.archive.clone(),
        );
    let upstream = MockServer::start().await;
    let explanation = json!({"error":{"type":"upstream_error","code":"NoAvailablePlan","message":"complete supplier explanation retained through restart"}});
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(402).set_body_json(&explanation))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model":fixture.model,"input":"summary contract","stream":false}),
        ),
    )
    .await
    .expect("client terminal precedes slow S3 publication");
    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("complete supplier explanation"));
    let (receipt_id, receipt) = wait_for_receipt(&fixture).await;
    let request_id = Uuid::parse_str(receipt["request_id"].as_str().unwrap()).unwrap();
    assert!(
        receipt["supplier_summary"]
            .as_str()
            .unwrap()
            .contains("complete supplier explanation")
    );
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let counts: (i64,i64) = sqlx::query_as("SELECT (SELECT COUNT(*) FROM proxy_operational_terminals WHERE request_id=$1), (SELECT COUNT(*) FROM conversation_projection_outbox WHERE request_id=$1)")
                .bind(request_id.to_string()).fetch_one(&pool).await.unwrap();
            if counts == (1,1) { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("SQL observation and conversation enqueue precede slow summary PUT");
    let requests = s3.received_requests().await.unwrap();
    assert!(
        !requests.is_empty(),
        "real S3 client reached blocked HTTP endpoint"
    );
    fixture.state.pending_terminals.stop_for_test().await;
    // The journal backend is independent of the unavailable summary endpoint.
    let location = format!(
        "terminal-receipts/v1/{}/{}.json",
        directory.file_name().unwrap().to_string_lossy(),
        receipt_id
    );
    let persisted: Value =
        serde_json::from_slice(&fixture.state.archive.get(&location).await.unwrap()).unwrap();
    assert_eq!(persisted["supplier_summary"], receipt["supplier_summary"]);
    s3.reset().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"summary\""))
        .mount(&s3)
        .await;
    fixture.state.pending_terminals =
        crate::proxy_lifecycle::pending_terminal::Journal::with_archive(
            directory,
            fixture.state.db.independent_persistence_pool(),
            fixture.state.archive.clone(),
        );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(request) = s3
                .received_requests()
                .await
                .unwrap()
                .into_iter()
                .find(|r| r.method.as_str() == "PUT")
            {
                assert_eq!(
                    request.body,
                    persisted["supplier_summary"].as_str().unwrap().as_bytes()
                );
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("restarted journal republishes complete explanation");
    let financial: (i64,i64) = sqlx::query_as("SELECT (SELECT COUNT(*) FROM usage_reservations WHERE id=(SELECT reservation_id FROM request_records WHERE id=$1)), (SELECT COUNT(*) FROM ledger_entries WHERE source=(SELECT reservation_id FROM request_records WHERE id=$1))")
        .bind(request_id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(
        financial,
        (1, 0),
        "unknown provider usage retains one reservation and no fabricated charge"
    );
    upstream.verify().await;
    fixture.state.pending_terminals.stop_for_test().await;
    fixture.state.db.close().await;
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
