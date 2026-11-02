use super::*;

#[tokio::test]
async fn durable_unknown_receipt_releases_projection_memory_without_discarding_evidence() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("{", "application/json"))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = response_usage_fixture("pending-unknown-memory", &upstream, 256).await;
    let held = fixture.state.pending_terminals.hold_writer_for_test().await;
    let response = send_response_usage_request(
        &fixture,
        &json!({
            "model": fixture.model, "input": "PRIVATE_OWNED_UNKNOWN_INPUT",
            "stream": false, "max_output_tokens": 16
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&delivered).contains("PRIVATE_OWNED_UNKNOWN_INPUT"));
    assert!(
        fixture.state.proxy_memory_budget.snapshot().0 > 0,
        "unacknowledged journal must retain its accounted payload"
    );
    drop(held);
    let receipt = cancellation::assert_single_unknown_receipt(
        &fixture,
        502,
        Some("upstream_invalid_response"),
    )
    .await;
    let receipt_id = Uuid::parse_str(receipt["receipt_id"].as_str().unwrap()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let (lifecycle, _, retained, _) = fixture.state.proxy_memory_budget.snapshot();
            if lifecycle == 0 && retained == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("durable unknown must release its raw projection and memory permits");
    let retained_receipt = fixture
        .state
        .pending_terminals
        .receipt_jsons_for_test()
        .into_iter()
        .find(|item| item["receipt_id"] == receipt["receipt_id"])
        .expect("unknown finance keeps the in-memory receipt");
    // Once the outbox row holds the payload the receipt drops the parsed
    // conversation from memory, so only the identity survives in-process.
    assert_eq!(retained_receipt["receipt_id"], receipt["receipt_id"]);
    assert_eq!(retained_receipt["request_id"], receipt["request_id"]);
    assert_eq!(retained_receipt["status_code"], receipt["status_code"]);
    assert_eq!(retained_receipt["error_code"], receipt["error_code"]);
    assert_eq!(retained_receipt["claim"], receipt["claim"]);
    assert_eq!(
        fixture
            .state
            .pending_terminals
            .idle_conversation_retained_for_test(receipt_id)
            .await,
        Some(false),
        "durable enqueue must drop the parsed conversation from memory"
    );
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let retained_body: String = sqlx::query_scalar(
        "SELECT request_json FROM conversation_projection_outbox WHERE request_id = $1 AND tenant_id = $2 AND key_id = $3 AND projected_at IS NULL",
    )
    .bind(receipt["request_id"].as_str().unwrap())
    .bind(receipt["claim"]["tenant_id"].as_str().unwrap())
    .bind(receipt["claim"]["key_id"].as_str().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&retained_body).unwrap()["input"],
        "PRIVATE_OWNED_UNKNOWN_INPUT"
    );
    pool.close().await;
    assert!(
        fixture
            .state
            .pending_terminals
            .contains_receipt_for_test(receipt_id)
    );
    cancellation::assert_unknown_request(
        &fixture,
        Uuid::parse_str(receipt["request_id"].as_str().unwrap()).unwrap(),
        0,
    )
    .await;
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    upstream.verify().await;
}

#[tokio::test]
async fn second_candidate_claim_failure_records_original_responsibility_without_waiting_for_ack() {
    let fixture = codex_route_fixture("pending-second-candidate-claim-failure").await;
    let standby = add_codex_standby_route(
        &fixture,
        "codex-route-pending-second-candidate-claim-failure",
        "account-456",
    )
    .await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header_matcher("chatgpt-account-id", "account-123"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": {"type": "rate_limit_exceeded"}
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(header_matcher("chatgpt-account-id", "account-456"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("must not dispatch"),
            "text/event-stream",
        ))
        .expect(0)
        .mount(&upstream)
        .await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_second_candidate_claim BEFORE UPDATE OF fence ON pending_proxy_settlements \
         WHEN NEW.contract_digest <> OLD.contract_digest \
         BEGIN SELECT RAISE(ABORT, 'CLAIM_FAILURE_PRIVATE_CANARY'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_claim_failure_operational BEFORE INSERT ON proxy_operational_terminals \
         BEGIN SELECT RAISE(ABORT, 'OPERATIONAL_FAILURE_PRIVATE_CANARY'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    let held = fixture.state.pending_terminals.hold_writer_for_test().await;
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model": fixture.model, "input": "second candidate claim", "stream": false}),
        ),
    )
    .await
    .expect("claim failure response must precede journal and operational ACK");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let request_id =
        Uuid::parse_str(response.headers()[REQUEST_ID_HEADER].to_str().unwrap()).unwrap();
    let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&delivered).unwrap();
    assert_eq!(body["error"]["code"], "database_operation_failed");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("数据库")
            || body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("内部错误")
    );
    assert!(!String::from_utf8_lossy(&delivered).contains("PRIVATE_CANARY"));
    let authority = sqlx::query(
        "SELECT p.fence, p.contract_digest, p.contract_json, q.upstream_account_id, q.completed_at, \
         (SELECT COUNT(*) FROM proxy_operational_terminals o WHERE o.request_id = q.id) AS operational_count \
         FROM pending_proxy_settlements p JOIN request_records q ON q.id = p.request_id WHERE p.request_id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    let contract: Value =
        serde_json::from_str(&authority.get::<String, _>("contract_json")).unwrap();
    assert_eq!(
        contract["upstream_account_id"],
        fixture.upstream_account_id.to_string()
    );
    assert_eq!(contract["model_route_id"], fixture.route_id.to_string());
    assert_eq!(
        authority.get::<String, _>("upstream_account_id"),
        standby.to_string()
    );
    assert_eq!(authority.get::<Option<i64>, _>("completed_at"), None);
    assert_eq!(authority.get::<i64, _>("operational_count"), 0);
    sqlx::query("DROP TRIGGER fail_claim_failure_operational")
        .execute(&pool)
        .await
        .unwrap();
    cancellation::assert_unknown_request(&fixture, request_id, 0).await;
    let record = wait_for_operational(&fixture).await;
    assert_eq!(record.status_code, Some(500));
    assert_eq!(
        record.error_code.as_deref(),
        Some("database_operation_failed")
    );
    assert_eq!(record.terminal_cause_code, None);
    assert_eq!(
        record.upstream_account_id,
        Some(fixture.upstream_account_id)
    );
    assert_eq!(record.route_id, Some(fixture.route_id));
    assert_eq!(
        record
            .operational_terminal
            .as_ref()
            .unwrap()
            .stage
            .as_deref(),
        Some("upstream_response_headers")
    );
    let operational_receipt: String = sqlx::query_scalar(
        "SELECT receipt_id FROM proxy_operational_terminals WHERE request_id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    let operational_receipt_id = Uuid::parse_str(&operational_receipt).unwrap();
    assert!(
        !fixture
            .state
            .pending_terminals
            .contains_receipt_for_test(operational_receipt_id)
    );
    drop(held);
    let receipt = cancellation::assert_unknown_receipt(
        &fixture,
        request_id,
        500,
        "database_operation_failed",
        0,
    )
    .await;
    assert_eq!(receipt["receipt_id"], operational_receipt);
    assert_eq!(
        receipt["claim"]["fence"],
        authority.get::<String, _>("fence")
    );
    assert_eq!(
        receipt["claim"]["contract_digest"],
        authority.get::<String, _>("contract_digest")
    );
    assert_eq!(receipt["input_token_ceiling"], contract["input_ceiling"]);
    assert_eq!(receipt["output_token_ceiling"], contract["output_ceiling"]);
    assert_eq!(
        receipt["reservation"]["reserved_micros"],
        contract["reserved_micros"]
    );
    assert_eq!(receipt["terminal_cause"], Value::Null);
    assert_eq!(
        receipt["operational_terminal"]["stage"],
        "upstream_response_headers"
    );
    assert!(
        fixture
            .state
            .pending_terminals
            .contains_receipt_for_test(operational_receipt_id),
        "runtime ACK must not retire unknown finance"
    );
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    upstream.verify().await;
    pool.close().await;
}

#[tokio::test]
async fn buffered_http_json_preserves_original_bytes_without_financial_ack() {
    let upstream = MockServer::start().await;
    let original = serde_json::to_vec_pretty(&completed_response_with_usage(3, 2)).unwrap();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(original.clone(), "application/json"))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = response_usage_fixture("pending-http-json", &upstream, 256).await;
    install_terminal_fault(&fixture).await;
    let held = fixture.state.pending_terminals.hold_writer_for_test().await;
    let response = tokio::time::timeout(Duration::from_secs(2), send_response_usage_request(
        &fixture,
        &json!({"model": fixture.model, "input": "buffered", "stream": false, "max_output_tokens": 16}),
    )).await.expect("ordinary buffered JSON must not wait for financial acknowledgement");
    assert_eq!(response.status(), StatusCode::OK);
    let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert_eq!(delivered.as_ref(), original.as_slice());
    drop(held);
    let (_, receipt) = wait_for_receipt(&fixture).await;
    assert_eq!(receipt["usage"]["input_tokens"], 3);
    assert_eq!(receipt["usage"]["output_tokens"], 2);
    assert_eq!(receipt["claim"]["request_id"], receipt["request_id"]);
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("DROP TRIGGER fail_pending_terminal")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (3, 2));
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-usage-contract"))
        .await;
    upstream.verify().await;
}

async fn wait_for_receipt(fixture: &CodexRouteFixture) -> (Uuid, Value) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(receipt) = fixture
                .state
                .pending_terminals
                .receipt_jsons_for_test()
                .into_iter()
                .next()
            {
                let receipt_id = Uuid::parse_str(receipt["receipt_id"].as_str().unwrap()).unwrap();
                return (receipt_id, receipt);
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("pending receipt must become durable")
}

async fn install_terminal_fault(fixture: &CodexRouteFixture) {
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_pending_terminal BEFORE UPDATE OF completed_at ON request_records \
         WHEN NEW.completed_at IS NOT NULL \
         BEGIN SELECT RAISE(ABORT, 'TERMINAL_FAULT_PRIVATE_CANARY'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}

async fn wait_for_operational(fixture: &CodexRouteFixture) -> crate::model::RequestView {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let mut rows = fixture
                .state
                .db
                .list_requests(fixture.key_id, 10)
                .await
                .unwrap();
            if rows.len() == 1 && rows[0].operational_terminal.is_some() {
                return rows.remove(0);
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("runtime completion must be visible independently of financial acknowledgement")
}

async fn assert_pending_usage(fixture: &CodexRouteFixture, receipt: &Value) {
    assert_eq!(receipt["usage"]["input_tokens"], 3);
    assert_eq!(receipt["usage"]["output_tokens"], 2);
    assert_eq!(receipt["usage_basis"], "provider_reported");
    assert_eq!(receipt["response_id"], "resp-codex");
    assert_eq!(receipt["claim"]["request_id"], receipt["request_id"]);
    assert_eq!(
        receipt["claim"]["reservation_id"],
        receipt["reservation"]["id"]
    );
    let record = wait_for_operational(fixture).await;
    assert_eq!(record.status_code, Some(200));
    assert_eq!(record.completed_at, None);
    assert_eq!(record.billing.cost, None);
    assert!(record.usage.tokens.is_none());
    assert_eq!(
        record.operational_terminal.unwrap().financial_state,
        crate::model::RequestFinancialState::Pending,
    );
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let ledger: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger_entries WHERE source = $1")
        .bind(receipt["reservation"]["id"].as_str().unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ledger, 0);
    pool.close().await;
}

async fn release_fault_and_check_once(fixture: &CodexRouteFixture) {
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("DROP TRIGGER fail_pending_terminal")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    wait_for_request_settlement(fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (3, 2));
    assert_exactly_once_side_effects(fixture, rows[0].request_id, Some("resp-codex")).await;
}

#[tokio::test]
async fn buffered_result_does_not_wait_for_journal_ack_or_terminal_sql() {
    let fixture = codex_route_fixture("buffered-pending-delivery").await;
    install_terminal_fault(&fixture).await;
    let held = fixture.state.pending_terminals.hold_writer_for_test().await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("original buffered output"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model": fixture.model, "input": "buffered", "stream": false}),
        ),
    )
    .await
    .expect("buffered output must not wait for financial acknowledgement");
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        body["output"][0]["content"][0]["text"],
        "original buffered output"
    );
    assert_eq!(body["usage"]["input_tokens"], 3);
    assert_eq!(body["usage"]["output_tokens"], 2);
    drop(held);
    let (receipt_id, receipt) = wait_for_receipt(&fixture).await;
    assert_pending_usage(&fixture, &receipt).await;
    release_fault_and_check_once(&fixture).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while fixture
            .state
            .pending_terminals
            .contains_receipt_for_test(receipt_id)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    upstream.verify().await;
}

async fn assert_streaming_faults(terminal_only: bool) {
    let fixture = codex_route_fixture(if terminal_only {
        "pending-terminal-only"
    } else {
        "pending-delta"
    })
    .await;
    let held = fixture.state.pending_terminals.hold_writer_for_test().await;
    let payload = if terminal_only {
        let response = json!({
            "type": "response.completed",
            "response": {"id": "resp-codex", "object": "response", "status": "completed",
                "output": [{"id": "item-codex", "type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": "original terminal output"}]}],
                "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}}
        });
        format!("event: response.completed\ndata: {response}\n\ndata: [DONE]\n\n")
    } else {
        streaming_codex_sse("original delta output", "original ", "delta output")
    };
    let (endpoint, release, upstream) =
        sse_delivery::gated_sse_upstream(payload.into_bytes()).await;
    let response = send_codex_route_to_endpoint(
        &fixture,
        endpoint,
        "/v1/responses",
        json!({"model": fixture.model, "input": "streaming", "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_post_dispatch_prepare BEFORE UPDATE OF error_code ON request_records \
         WHEN NEW.error_code IN ('delivery_prepared', 'delivery_started') \
         BEGIN SELECT RAISE(ABORT, 'DELIVERY_FAULT_PRIVATE_CANARY'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    install_terminal_fault(&fixture).await;
    release.send(()).unwrap();
    let delivered = tokio::time::timeout(
        Duration::from_secs(2),
        to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY),
    )
    .await
    .expect("ordinary and terminal-only output must not wait for SQL or journal ACK")
    .unwrap();
    let delivered = String::from_utf8(delivered.to_vec()).unwrap();
    assert!(delivered.contains("response.completed"));
    assert!(delivered.contains(if terminal_only {
        "original terminal output"
    } else {
        "original delta output"
    }));
    assert!(!delivered.contains("upstream request failed"));
    drop(held);
    let (_, receipt) = wait_for_receipt(&fixture).await;
    assert_eq!(receipt["queued_billable"], true);
    assert_pending_usage(&fixture, &receipt).await;
    release_fault_and_check_once(&fixture).await;
    upstream.await.unwrap();
}

#[tokio::test]
async fn first_billable_delta_survives_post_dispatch_delivery_and_terminal_sql_faults() {
    assert_streaming_faults(false).await;
}

#[tokio::test]
async fn terminal_only_output_survives_post_dispatch_delivery_and_terminal_sql_faults() {
    assert_streaming_faults(true).await;
}

#[tokio::test]
async fn durable_receipt_recovers_after_worker_replacement_with_original_usage() {
    let fixture = codex_route_fixture("pending-worker-recovery").await;
    install_terminal_fault(&fixture).await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("recover original usage"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "recovery", "stream": false}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let (receipt_id, original) = wait_for_receipt(&fixture).await;
    assert_pending_usage(&fixture, &original).await;
    let original_operational = wait_for_operational(&fixture)
        .await
        .operational_terminal
        .unwrap();
    release_fault_and_check_once(&fixture).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while fixture
            .state
            .pending_terminals
            .contains_receipt_for_test(receipt_id)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let replayed_operational = wait_for_operational(&fixture)
        .await
        .operational_terminal
        .unwrap();
    assert_eq!(
        replayed_operational.completed_at,
        original_operational.completed_at
    );
    assert_eq!(replayed_operational.stage, original_operational.stage);
    assert_eq!(
        replayed_operational.financial_state,
        crate::model::RequestFinancialState::Applied
    );
    upstream.verify().await;
}

#[tokio::test]
async fn cancellation_without_provider_usage_preserves_registered_unknown_hold() {
    let fixture = codex_route_fixture("pending-cancel-unknown").await;
    let (endpoint, release, upstream) = sse_delivery::gated_sse_upstream(
        completed_codex_sse("not observed by gateway").into_bytes(),
    )
    .await;
    let response = send_codex_route_to_endpoint(
        &fixture,
        endpoint,
        "/v1/responses",
        json!({"model": fixture.model, "input": "cancel before usage", "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
            let row = sqlx::query(
                "SELECT q.completed_at, r.status, p.state, p.unknown_reason, \
                 (SELECT COUNT(*) FROM ledger_entries l WHERE l.source = r.id) AS ledger_count \
                 FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id \
                 JOIN pending_proxy_settlements p ON p.request_id = q.id WHERE q.key_id = $1",
            )
            .bind(fixture.key_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
            pool.close().await;
            if row.get::<Option<String>, _>("unknown_reason").is_some() {
                assert_eq!(row.get::<Option<i64>, _>("completed_at"), None);
                assert_eq!(row.get::<String, _>("status"), "reserved");
                assert_eq!(row.get::<String, _>("state"), "pending");
                assert_eq!(row.get::<i64, _>("ledger_count"), 0);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancellation must remain explicit unknown rather than zero usage");
    let receipt =
        cancellation::assert_single_unknown_receipt(&fixture, 499, Some("client_cancelled")).await;
    assert_eq!(
        receipt["operational_terminal"]["stage"],
        "response_delivery"
    );
    assert_eq!(receipt["terminal_cause"], "client_disconnected");
    drop(release);
    upstream.abort();
    let _ = upstream.await;
}

#[tokio::test]
async fn committed_receipt_replays_after_worker_loss_before_retirement_without_duplicate_charge() {
    assert_committed_receipt_replay_preserves_owned_key(None).await;
}

#[tokio::test]
async fn provider_reported_authority_arbitration_keeps_durable_bytes_and_releases_memory() {
    for already_finished in [false, true] {
        let fixture = codex_route_fixture("pending-authority-memory").await;
        let held = fixture.state.pending_terminals.hold_writer_for_test().await;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(codex_transport::RESPONSES_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                completed_codex_sse("authority conflict retains original usage"),
                "text/event-stream",
            ))
            .expect(1)
            .mount(&upstream)
            .await;
        let response = send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model": fixture.model, "input": "authority conflict", "stream": false}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&delivered)
                .contains("authority conflict retains original usage")
        );
        let record = wait_for_operational(&fixture).await;
        let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
        if already_finished {
            sqlx::query("UPDATE request_records SET completed_at = $1, status_code = 504, error_code = 'request_expired' WHERE id = $2")
                .bind(crate::db::unix_millis()).bind(record.request_id.to_string())
                .execute(&pool).await.unwrap();
        } else {
            sqlx::query("UPDATE pending_proxy_settlements SET fence = $1 WHERE request_id = $2")
                .bind(Uuid::now_v7().to_string())
                .bind(record.request_id.to_string())
                .execute(&pool)
                .await
                .unwrap();
        }
        drain_archive_capture(&fixture).await;
        assert!(fixture.state.proxy_memory_budget.snapshot().0 > 0);
        drop(held);
        let (receipt_id, receipt) = wait_for_receipt(&fixture).await;
        assert_eq!(receipt["usage_basis"], "provider_reported");
        assert_eq!(receipt["usage"]["input_tokens"], 3);
        assert_eq!(receipt["usage"]["output_tokens"], 2);
        assert!(receipt["conversation"].is_object());
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if fixture.state.pending_terminals.requires_arbitration_for_test(receipt_id)
                    && fixture.state.pending_terminals.idle_conversation_retained_for_test(receipt_id).await == Some(false)
                {
                    let (used, _, retained, _) = fixture.state.proxy_memory_budget.snapshot();
                    if used == 0 && retained == 0 {
                        break;
                    }
                }
                tokio::task::yield_now().await;
            }
        }).await.expect("durable authority arbitration must release payload and memory without retiring evidence");
        assert!(
            fixture
                .state
                .pending_terminals
                .contains_receipt_for_test(receipt_id)
        );
        let financial: (String, Option<String>, String, Option<i64>, i64, i64) = sqlx::query_as(
            "SELECT p.state, p.receipt_id, r.status, r.actual_micros, \
             (SELECT COUNT(*) FROM ledger_entries WHERE source = r.id), \
             (SELECT COUNT(*) FROM request_events WHERE request_id = p.request_id AND event_kind = 'finished') \
             FROM pending_proxy_settlements p JOIN usage_reservations r ON r.id = p.reservation_id WHERE p.request_id = $1",
        ).bind(record.request_id.to_string()).fetch_one(&pool).await.unwrap();
        assert_eq!(
            financial,
            (
                "pending".to_owned(),
                None,
                "reserved".to_owned(),
                None,
                0,
                0
            )
        );
        assert_eq!(fixture.state.proxy_lifecycles.active(), 0);
        pool.close().await;
        upstream.verify().await;
    }
}

#[tokio::test]
async fn changed_owned_key_receipt_conflicts_and_is_not_retired_after_commit() {
    for field in ["alias", "currency", "credential_generation", "policy"] {
        assert_committed_receipt_replay_preserves_owned_key(Some(field)).await;
    }
}

#[tokio::test]
async fn changed_operational_observation_is_not_retired_by_a_matching_financial_ack() {
    assert_committed_receipt_replay_preserves_owned_key(Some("operational_terminal")).await;
}

async fn assert_committed_receipt_replay_preserves_owned_key(changed_field: Option<&str>) {
    let fixture = codex_route_fixture("pending-ack-recovery").await;
    let held = fixture
        .state
        .pending_terminals
        .hold_retirement_for_test()
        .await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("already financially committed"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "ACK recovery", "stream": false}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let (receipt_id, receipt) = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(receipt) = fixture
                .state
                .pending_terminals
                .receipt_jsons_for_test()
                .into_iter()
                .next()
                && receipt["conversation"]["key"].is_object()
            {
                let receipt_id = Uuid::parse_str(receipt["receipt_id"].as_str().unwrap()).unwrap();
                return (receipt_id, receipt);
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owned key receipt must include the conversation key");
    wait_for_request_settlement(&fixture, 1).await;
    let operational = wait_for_operational(&fixture)
        .await
        .operational_terminal
        .unwrap();
    assert_eq!(
        json!(operational.completed_at),
        receipt["operational_terminal"]["completed_at"]
    );
    drop(held);
    let _ = changed_field;
    tokio::time::timeout(Duration::from_secs(5), async {
        while fixture
            .state
            .pending_terminals
            .contains_receipt_for_test(receipt_id)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("an in-memory receipt retires after financial and runtime acknowledgement");
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(
        rows[0].request_id.to_string(),
        receipt["request_id"].as_str().unwrap()
    );
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (3, 2));
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-codex")).await;
    upstream.verify().await;
}

#[tokio::test]
async fn already_finished_without_matching_receipt_ack_keeps_durable_evidence() {
    let fixture = codex_route_fixture("pending-unmatched-terminal").await;
    install_terminal_fault(&fixture).await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("must preserve financial evidence"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    // The journal must not settle in the gap after the fault trigger is gone
    // and before this test records the unmatched terminal.
    let held = fixture.state.pending_terminals.hold_writer_for_test().await;
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model": fixture.model, "input": "unmatched ACK", "stream": false}),
        ),
    )
    .await
    .expect("delivery must not wait for the held journal");
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let (receipt_id, receipt) = wait_for_receipt(&fixture).await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("DROP TRIGGER fail_pending_terminal")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE request_records SET completed_at = $1, status_code = 504, error_code = 'request_expired' WHERE id = $2")
        .bind(crate::db::unix_millis()).bind(receipt["request_id"].as_str().unwrap())
        .execute(&pool).await.unwrap();
    pool.close().await;
    drop(held);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !fixture
            .state
            .pending_terminals
            .requires_arbitration_for_test(receipt_id)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("unmatched existing terminal must require explicit arbitration");
    assert_eq!(
        fixture
            .state
            .pending_terminals
            .receipt_jsons_for_test()
            .into_iter()
            .find(|item| item["receipt_id"] == receipt["receipt_id"])
            .unwrap(),
        receipt
    );
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let terminal: (Option<i64>, Option<String>, i64) = sqlx::query_as(
        "SELECT status_code, error_code, cost_micros FROM request_records WHERE id = $1",
    )
    .bind(receipt["request_id"].as_str().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(terminal, (Some(504), Some("request_expired".into()), 0));
    let authority: (String, Option<String>) = sqlx::query_as(
        "SELECT state, receipt_id FROM pending_proxy_settlements WHERE request_id = $1",
    )
    .bind(receipt["request_id"].as_str().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(authority, ("pending".into(), None));
    pool.close().await;
    upstream.verify().await;
}

#[tokio::test]
async fn first_billable_delta_arrives_while_upstream_terminal_and_journal_ack_are_held() {
    let fixture = codex_route_fixture("pending-first-delta-before-terminal").await;
    let held = fixture.state.pending_terminals.hold_writer_for_test().await;
    install_terminal_fault(&fixture).await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_delivery_observation BEFORE UPDATE OF error_code ON request_records \
         WHEN NEW.error_code IN ('delivery_prepared', 'delivery_started') \
         BEGIN SELECT RAISE(ABORT, 'DELIVERY_OBSERVATION_PRIVATE_CANARY'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    let payload = streaming_codex_sse("early delta", "early ", "delta");
    let mut events = payload.split_inclusive("\n\n");
    let prefix: String = events.by_ref().take(3).collect();
    let terminal: String = events.collect();
    let (endpoint, release, upstream) =
        gated_completed_sse_upstream_endpoint(prefix, terminal).await;
    let response = send_codex_route_to_endpoint(
        &fixture,
        endpoint,
        "/v1/responses",
        json!({"model": fixture.model, "input": "first delta", "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.into_body().into_data_stream();
    let mut delivered = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !String::from_utf8_lossy(&delivered).contains("early ") {
            delivered.extend_from_slice(&stream.next().await.unwrap().unwrap());
        }
    })
    .await
    .expect("billable delta must precede upstream terminal and financial ACK");
    assert!(!String::from_utf8_lossy(&delivered).contains("response.completed"));
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(chunk) = stream.next().await {
            delivered.extend_from_slice(&chunk.unwrap());
        }
    })
    .await
    .expect("EOF must not wait for journal or settlement");
    assert!(String::from_utf8_lossy(&delivered).contains("response.completed"));
    drop(held);
    let (_, receipt) = wait_for_receipt(&fixture).await;
    assert_pending_usage(&fixture, &receipt).await;
    release_fault_and_check_once(&fixture).await;
    upstream.await.unwrap();
}

#[tokio::test]
async fn unavailable_journal_keeps_output_and_explicit_unacknowledged_financial_responsibility() {
    let mut fixture = codex_route_fixture("pending-filesystem-fault").await;
    let unavailable = fixture._directory.path().join("journal-is-a-file");
    tokio::fs::write(&unavailable, b"not a directory")
        .await
        .unwrap();
    fixture.state.pending_terminals = crate::proxy_lifecycle::pending_terminal::Journal::new(
        unavailable.clone(),
        fixture.state.db.independent_persistence_pool(),
    );
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("output despite unacknowledged journal"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model": fixture.model, "input": "filesystem failure", "stream": false}),
        ),
    )
    .await
    .expect("journal failure must not hold generated output");
    assert_eq!(response.status(), StatusCode::OK);
    let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&delivered).contains("output despite unacknowledged journal"));
    assert_eq!(
        fixture
            .state
            .pending_terminals
            .failed_publications_for_test(),
        0
    );
    wait_for_request_settlement(&fixture, 1).await;
    let record = wait_for_operational(&fixture).await;
    assert_eq!(record.status_code, Some(200));
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    tokio::fs::remove_file(unavailable).await.unwrap();
    let recovered = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(recovered[0].request_id, record.request_id);
    assert_eq!(
        (recovered[0].input_tokens, recovered[0].output_tokens),
        (3, 2)
    );
    assert_exactly_once_side_effects(&fixture, record.request_id, Some("resp-codex")).await;
    pool.close().await;
    fixture.state.pending_terminals.stop_for_test().await;
    upstream.verify().await;
}

#[tokio::test]
async fn operational_database_failure_does_not_block_finance_and_replays_before_retirement() {
    let mut fixture = codex_route_fixture("pending-operational-recovery").await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_operational_terminal BEFORE INSERT ON proxy_operational_terminals \
         BEGIN SELECT RAISE(ABORT, 'OPERATIONAL_FAULT_PRIVATE_CANARY'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("output before operational ACK"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model": fixture.model, "input": "operational retry", "stream": false}),
        ),
    )
    .await
    .expect("operational SQL must not block output");
    assert_eq!(response.status(), StatusCode::OK);
    let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&delivered).contains("output before operational ACK"));
    assert!(!String::from_utf8_lossy(&delivered).contains("OPERATIONAL_FAULT_PRIVATE_CANARY"));
    let (receipt_id, receipt) = wait_for_receipt(&fixture).await;
    wait_for_request_settlement(&fixture, 1).await;
    let request_id = Uuid::parse_str(receipt["request_id"].as_str().unwrap()).unwrap();
    assert_exactly_once_side_effects(&fixture, request_id, Some("resp-codex")).await;
    assert!(
        fixture
            .state
            .pending_terminals
            .contains_receipt_for_test(receipt_id),
        "financial ACK must not discard an unprojected runtime receipt"
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM proxy_operational_terminals WHERE request_id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 0);
    let directory = fixture
        .state
        .pending_terminals
        .directory_for_test()
        .to_path_buf();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !directory.join(format!("{receipt_id}.json")).is_file() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("unprojected financial ACK must retain a compact recovery receipt");
    let compact: Value = serde_json::from_slice(
        &tokio::fs::read(directory.join(format!("{receipt_id}.json")))
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(compact["conversation"].is_null());
    assert_eq!(compact["usage"], receipt["usage"]);
    assert_eq!(
        compact["financial_ack"]["receipt_id"],
        receipt["receipt_id"]
    );
    assert_eq!(
        compact["financial_ack"]["request_id"],
        receipt["request_id"]
    );
    assert!(
        compact["financial_ack"]["receipt_digest"]
            .as_str()
            .is_some()
    );
    fixture.state.pending_terminals.stop_for_test().await;
    sqlx::query("DROP TRIGGER fail_operational_terminal")
        .execute(&pool)
        .await
        .unwrap();
    fixture.state.pending_terminals = crate::proxy_lifecycle::pending_terminal::Journal::new(
        directory.clone(),
        fixture.state.db.independent_persistence_pool(),
    );
    let record = wait_for_operational(&fixture).await;
    let operational = record.operational_terminal.unwrap();
    assert_eq!(
        json!(operational.completed_at),
        receipt["operational_terminal"]["completed_at"]
    );
    assert_eq!(
        json!(operational.stage),
        receipt["operational_terminal"]["stage"]
    );
    assert_eq!(
        operational.financial_state,
        crate::model::RequestFinancialState::Applied
    );
    assert_eq!(record.status_code, Some(200));
    tokio::time::timeout(Duration::from_secs(3), async {
        while fixture
            .state
            .pending_terminals
            .contains_receipt_for_test(receipt_id)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!directory.join(format!("{receipt_id}.json")).exists());
    assert_exactly_once_side_effects(&fixture, request_id, Some("resp-codex")).await;
    pool.close().await;
    upstream.verify().await;
}

#[tokio::test]
async fn unknown_failure_retries_operational_sql_after_financial_arbitration_without_replay() {
    unknown_failure_retries_operational_projection().await;
}

async fn unknown_failure_retries_operational_projection() {
    let fixture = codex_route_fixture("pending-unknown-operational-retry").await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_unknown_operational BEFORE INSERT ON proxy_operational_terminals \
         BEGIN SELECT RAISE(ABORT, 'UNKNOWN_OPERATIONAL_PRIVATE_CANARY'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_unknown_projection BEFORE INSERT ON conversation_projection_outbox \
         BEGIN SELECT RAISE(ABORT, 'UNKNOWN_PROJECTION_PRIVATE_CANARY'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            concat!(
                "event: response.failed\n",
                "data: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp-failed\",\"error\":{\"message\":\"PROVIDER_PRIVATE_CANARY\"}}}\n\n",
                "data: [DONE]\n\n",
            ),
            "text/event-stream",
        ))
        .expect(1).mount(&upstream).await;
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model": fixture.model, "input": "operational failure retry", "stream": true}),
        ),
    )
    .await
    .expect("runtime SQL failure must not delay response headers");
    assert_eq!(response.status(), StatusCode::OK);
    let body = tokio::time::timeout(
        Duration::from_secs(2),
        to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY),
    )
    .await
    .expect("runtime SQL failure must not delay delivered failure")
    .unwrap();
    let delivered = String::from_utf8(body.to_vec()).unwrap();
    assert_eq!(delivered.matches("event: response.failed").count(), 1);
    assert!(!delivered.contains("response.completed"));
    assert!(!delivered.contains("PRIVATE_CANARY"));
    let original = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let (_, receipt) = wait_for_receipt(&fixture).await;
            if receipt["conversation"].is_object() {
                break receipt;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("asynchronous preparation must retain the original conversation");
    let receipt_id = Uuid::parse_str(original["receipt_id"].as_str().unwrap()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !fixture
            .state
            .pending_terminals
            .requires_arbitration_for_test(receipt_id)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("unknown finance must retain its durable receipt");
    let request_id = Uuid::parse_str(original["request_id"].as_str().unwrap()).unwrap();
    assert!(original["conversation"].is_object());
    tokio::time::timeout(Duration::from_secs(3), async {
        while fixture
            .state
            .pending_terminals
            .idle_conversation_retained_for_test(receipt_id)
            .await
            != Some(true)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("unknown awaiting its operational receipt must survive idle reap");
    assert!(fixture.state.proxy_memory_budget.snapshot().0 > 0);
    let financial: (String, String, Option<i64>, i64, i64) = sqlx::query_as(
        "SELECT p.unknown_reason, r.status, r.actual_micros, \
         (SELECT COUNT(*) FROM ledger_entries WHERE source = r.id), \
         (SELECT COUNT(*) FROM request_events WHERE request_id = p.request_id AND event_kind = 'finished') \
         FROM pending_proxy_settlements p JOIN usage_reservations r ON r.id = p.reservation_id WHERE p.request_id = $1",
    ).bind(request_id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(
        financial,
        (
            "authoritative_usage_unavailable".to_owned(),
            "reserved".to_owned(),
            None,
            0,
            0
        )
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM proxy_operational_terminals WHERE request_id = $1",
    )
    .bind(original["request_id"].as_str().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 0);
    sqlx::query("DROP TRIGGER fail_unknown_operational")
        .execute(&pool)
        .await
        .unwrap();
    let receipt = cancellation::assert_single_unknown_receipt(
        &fixture,
        502,
        Some("upstream_failed_response"),
    )
    .await;
    // The retried pass may have enqueued the conversation and reaped the
    // parsed copy already; the receipt's identity must be unchanged either
    // way.
    assert_eq!(receipt["receipt_id"], original["receipt_id"]);
    assert_eq!(receipt["request_id"], original["request_id"]);
    assert_eq!(receipt["status_code"], original["status_code"]);
    assert_eq!(receipt["error_code"], original["error_code"]);
    assert_eq!(receipt["claim"], original["claim"]);
    assert!(
        fixture
            .state
            .pending_terminals
            .contains_receipt_for_test(receipt_id)
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while fixture
            .state
            .pending_terminals
            .idle_conversation_retained_for_test(receipt_id)
            .await
            != Some(true)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("operational unknown acknowledgement must not discard a still-unqueued conversation");
    let enqueued: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_projection_outbox WHERE request_id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(enqueued, 0);
    assert!(fixture.state.proxy_memory_budget.snapshot().0 > 0);
    sqlx::query("DROP TRIGGER fail_unknown_projection")
        .execute(&pool)
        .await
        .unwrap();
    let payload: String = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(payload) = sqlx::query_scalar::<_, String>(
                "SELECT request_json FROM conversation_projection_outbox WHERE request_id = $1",
            )
            .bind(request_id.to_string())
            .fetch_optional(&pool)
            .await
            .unwrap()
            {
                break payload;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("financial arbitration must keep retrying the original semantic projection");
    assert_eq!(
        serde_json::from_str::<Value>(&payload).unwrap(),
        original["conversation"]["request_json"]
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if fixture
                .state
                .pending_terminals
                .idle_conversation_retained_for_test(receipt_id)
                .await
                == Some(false)
            {
                let (used, _, retained, _) = fixture.state.proxy_memory_budget.snapshot();
                if used == 0 && retained == 0 {
                    break;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect(
        "unknown with durable semantic enqueue must release memory and the parsed conversation",
    );
    let owner = Uuid::now_v7();
    let tasks = fixture
        .state
        .db
        .claim_conversation_projection_tasks(owner, 10)
        .await
        .unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].request_id, request_id);
    assert!(
        fixture
            .state
            .db
            .project_claimed_conversation_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    assert!(
        !fixture
            .state
            .db
            .project_claimed_conversation_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    let effects: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM conversation_observations WHERE request_id = $1), \
         (SELECT COUNT(*) FROM ledger_entries WHERE source = $2), \
         (SELECT COUNT(*) FROM request_events WHERE request_id = $1 AND event_kind = 'finished')",
    )
    .bind(request_id.to_string())
    .bind(original["reservation"]["id"].as_str().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(effects, (1, 0, 0));
    pool.close().await;
    upstream.verify().await;
}

#[tokio::test]
async fn runtime_ack_survives_unknown_annotation_failure_before_journal_publication() {
    let fixture = codex_route_fixture("pending-unknown-annotation-retry").await;
    let held = fixture.state.pending_terminals.hold_writer_for_test().await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_unknown_annotation BEFORE UPDATE OF unknown_reason ON pending_proxy_settlements \
         WHEN NEW.unknown_reason IS NOT NULL \
         BEGIN SELECT RAISE(ABORT, 'UNKNOWN_ANNOTATION_PRIVATE_CANARY'); END",
    ).execute(&pool).await.unwrap();
    let (endpoint, release, server) = sse_delivery::gated_sse_upstream(
        completed_codex_sse("provider terminal never observed").into_bytes(),
    )
    .await;
    let response = send_codex_route_to_endpoint(
        &fixture,
        endpoint,
        "/v1/responses",
        json!({"model": fixture.model, "input": "cancel before provider terminal", "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);
    let record = wait_for_operational(&fixture).await;
    assert_eq!(record.status_code, Some(499));
    assert_eq!(record.error_code.as_deref(), Some("client_cancelled"));
    assert_eq!(
        record.terminal_cause_code.as_deref(),
        Some("client_disconnected")
    );
    assert!(record.usage.tokens.is_none());
    assert_eq!(record.billing.cost, None);
    let operational = record.operational_terminal.unwrap();
    assert_eq!(
        operational.financial_state,
        crate::model::RequestFinancialState::Pending
    );
    let original: (String, String, i64) = sqlx::query_as(
        "SELECT receipt_id, operational_digest, completed_at FROM proxy_operational_terminals WHERE request_id = $1",
    ).bind(record.request_id.to_string()).fetch_one(&pool).await.unwrap();
    let receipt_id = Uuid::parse_str(&original.0).unwrap();
    assert!(
        !fixture
            .state
            .pending_terminals
            .contains_receipt_for_test(receipt_id),
        "runtime ACK cannot require journal publication"
    );
    sqlx::query("DROP TRIGGER fail_unknown_annotation")
        .execute(&pool)
        .await
        .unwrap();
    cancellation::assert_unknown_request(&fixture, record.request_id, 0).await;
    let replayed: (String, String, i64) = sqlx::query_as(
        "SELECT receipt_id, operational_digest, completed_at FROM proxy_operational_terminals WHERE request_id = $1",
    ).bind(record.request_id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(replayed, original);
    assert!(
        !fixture
            .state
            .pending_terminals
            .contains_receipt_for_test(receipt_id)
    );
    drop(held);
    let receipt =
        cancellation::assert_single_unknown_receipt(&fixture, 499, Some("client_cancelled")).await;
    assert_eq!(receipt["receipt_id"], original.0);
    assert_eq!(receipt["operational_terminal"]["completed_at"], original.2);
    // The recovered annotation enqueues the conversation as durable
    // evidence; the parsed copy leaves memory once that lands.
    let enqueued: String = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(payload) = sqlx::query_scalar::<_, String>(
                "SELECT request_json FROM conversation_projection_outbox WHERE request_id = $1",
            )
            .bind(record.request_id.to_string())
            .fetch_optional(&pool)
            .await
            .unwrap()
            {
                break payload;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("runtime ACK recovery must enqueue the conversation as durable evidence");
    assert_eq!(
        serde_json::from_str::<Value>(&enqueued).unwrap()["input"],
        "cancel before provider terminal"
    );
    assert!(
        fixture
            .state
            .pending_terminals
            .contains_receipt_for_test(receipt_id),
        "runtime ACK is not a financial receipt ACK"
    );
    release.send(()).unwrap();
    server.await.unwrap();
    drain_archive_capture(&fixture).await;
    let receipt_id = Uuid::parse_str(&original.0).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let (used, _, retained, _) = fixture.state.proxy_memory_budget.snapshot();
            if used == 0
                && retained == 0
                && fixture
                    .state
                    .pending_terminals
                    .idle_conversation_retained_for_test(receipt_id)
                    .await
                    == Some(false)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect(
        "durable semantic enqueue must release every memory permit and drop the parsed conversation",
    );
    assert!(
        fixture
            .state
            .pending_terminals
            .contains_receipt_for_test(receipt_id)
    );
    pool.close().await;
}
