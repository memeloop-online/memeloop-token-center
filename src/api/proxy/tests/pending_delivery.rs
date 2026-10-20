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
    assert_eq!(
        receipt["conversation"]["request_json"]["input"],
        "PRIVATE_OWNED_UNKNOWN_INPUT"
    );
    let path = fixture
        .state
        .pending_terminals
        .directory_for_test()
        .join(format!("{}.json", receipt["receipt_id"].as_str().unwrap()));
    let durable = tokio::fs::read(&path).await.unwrap();
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
    assert_eq!(tokio::fs::read(&path).await.unwrap(), durable);
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
    assert_eq!(body["error"]["code"], "internal_error");
    assert_eq!(body["error"]["message"], "internal error");
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
    assert_eq!(record.error_code.as_deref(), Some("internal_error"));
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
    let path = fixture
        .state
        .pending_terminals
        .directory_for_test()
        .join(format!("{operational_receipt}.json"));
    assert!(!path.exists());
    drop(held);
    let receipt =
        cancellation::assert_unknown_receipt(&fixture, request_id, 500, "internal_error", 0).await;
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
    assert!(path.exists(), "runtime ACK must not retire unknown finance");
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

async fn wait_for_receipt(fixture: &CodexRouteFixture) -> (std::path::PathBuf, Value) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(mut entries) =
                tokio::fs::read_dir(fixture.state.pending_terminals.directory_for_test()).await
            {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                        continue;
                    }
                    if let Ok(bytes) = tokio::fs::read(entry.path()).await
                        && let Ok(receipt) = serde_json::from_slice::<Value>(&bytes)
                    {
                        return (entry.path(), receipt);
                    }
                }
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
    let (path, receipt) = wait_for_receipt(&fixture).await;
    assert_pending_usage(&fixture, &receipt).await;
    release_fault_and_check_once(&fixture).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while path.exists() {
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
    let mut fixture = codex_route_fixture("pending-worker-recovery").await;
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
    let (path, original) = wait_for_receipt(&fixture).await;
    assert_pending_usage(&fixture, &original).await;
    let original_operational = wait_for_operational(&fixture)
        .await
        .operational_terminal
        .unwrap();
    let directory = fixture
        .state
        .pending_terminals
        .directory_for_test()
        .to_path_buf();
    fixture.state.pending_terminals.stop_for_test().await;
    fixture.state.pending_terminals = crate::proxy_lifecycle::pending_terminal::Journal::new(
        directory,
        fixture.state.db.independent_persistence_pool(),
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&tokio::fs::read(&path).await.unwrap()).unwrap(),
        original
    );
    release_fault_and_check_once(&fixture).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while path.exists() {
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
    let mut fixture = codex_route_fixture("pending-ack-recovery").await;
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
    let (path, receipt) = wait_for_receipt(&fixture).await;
    assert!(receipt["conversation"]["key"].is_object());
    wait_for_request_settlement(&fixture, 1).await;
    let operational = wait_for_operational(&fixture)
        .await
        .operational_terminal
        .unwrap();
    assert_eq!(
        json!(operational.completed_at),
        receipt["operational_terminal"]["completed_at"]
    );
    let directory = fixture
        .state
        .pending_terminals
        .directory_for_test()
        .to_path_buf();
    fixture.state.pending_terminals.stop_for_test().await;
    drop(held);
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let changed_policy = crate::model::KeyPolicy {
        requests_per_minute: 7,
        ..Default::default()
    };
    sqlx::query("UPDATE key_records SET alias = 'changed after terminal', currency = 'EUR', credential_generation = credential_generation + 1, policy_json = $1 WHERE id = $2")
        .bind(serde_json::to_string(&changed_policy).unwrap())
        .bind(fixture.key_id.to_string())
        .execute(&pool).await.unwrap();
    pool.close().await;
    let mut replayed_receipt = receipt.clone();
    if let Some(field) = changed_field {
        let key = &mut replayed_receipt["conversation"]["key"];
        match field {
            "alias" => key["alias"] = json!("changed receipt alias"),
            "currency" => key["currency"] = json!("EUR"),
            "credential_generation" => {
                key["credential_generation"] =
                    json!(key["credential_generation"].as_i64().unwrap() + 1);
            }
            "policy" => {
                key["policy"]["requests_per_minute"] =
                    json!(key["policy"]["requests_per_minute"].as_u64().unwrap() + 1);
            }
            "operational_terminal" => {
                replayed_receipt["operational_terminal"]["completed_at"] = json!(
                    receipt["operational_terminal"]["completed_at"]
                        .as_i64()
                        .unwrap()
                        + 1
                );
            }
            _ => unreachable!(),
        }
        tokio::fs::write(&path, serde_json::to_vec(&replayed_receipt).unwrap())
            .await
            .unwrap();
    }
    fixture.state.pending_terminals = crate::proxy_lifecycle::pending_terminal::Journal::new(
        directory,
        fixture.state.db.independent_persistence_pool(),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        if changed_field.is_some() {
            let receipt_id = Uuid::parse_str(receipt["receipt_id"].as_str().unwrap()).unwrap();
            while !fixture
                .state
                .pending_terminals
                .requires_arbitration_for_test(receipt_id)
            {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                serde_json::from_slice::<Value>(&tokio::fs::read(&path).await.unwrap()).unwrap(),
                replayed_receipt
            );
        } else {
            while path.exists() {
                tokio::task::yield_now().await;
            }
        }
    })
    .await
    .expect("only an unchanged owned receipt may receive the matching replay ACK");
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
    let mut fixture = codex_route_fixture("pending-unmatched-terminal").await;
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
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "unmatched ACK", "stream": false}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let (path, receipt) = wait_for_receipt(&fixture).await;
    let directory = fixture
        .state
        .pending_terminals
        .directory_for_test()
        .to_path_buf();
    fixture.state.pending_terminals.stop_for_test().await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("DROP TRIGGER fail_pending_terminal")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE request_records SET completed_at = $1, status_code = 504, error_code = 'request_expired' WHERE id = $2")
        .bind(crate::db::unix_millis()).bind(receipt["request_id"].as_str().unwrap())
        .execute(&pool).await.unwrap();
    pool.close().await;
    fixture.state.pending_terminals = crate::proxy_lifecycle::pending_terminal::Journal::new(
        directory,
        fixture.state.db.independent_persistence_pool(),
    );
    let receipt_id = Uuid::parse_str(receipt["receipt_id"].as_str().unwrap()).unwrap();
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
        serde_json::from_slice::<Value>(&tokio::fs::read(path).await.unwrap()).unwrap(),
        receipt
    );
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
    tokio::time::timeout(Duration::from_secs(3), async {
        while fixture
            .state
            .pending_terminals
            .failed_publications_for_test()
            == 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed publication must leave an explicit diagnostic");
    let record = wait_for_operational(&fixture).await;
    assert_eq!(record.status_code, Some(200));
    assert_eq!(record.completed_at, None);
    assert_eq!(record.billing.cost, None);
    assert!(record.usage.tokens.is_none());
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let state: String =
        sqlx::query_scalar("SELECT state FROM pending_proxy_settlements WHERE request_id = $1")
            .bind(record.request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "pending");
    let charges: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM ledger_entries WHERE kind = 'usage'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(charges, 0);
    assert!(fixture.state.proxy_memory_budget.snapshot().0 > 0);
    tokio::fs::remove_file(unavailable).await.unwrap();
    wait_for_request_settlement(&fixture, 1).await;
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
    let (path, receipt) = wait_for_receipt(&fixture).await;
    wait_for_request_settlement(&fixture, 1).await;
    let request_id = Uuid::parse_str(receipt["request_id"].as_str().unwrap()).unwrap();
    assert_exactly_once_side_effects(&fixture, request_id, Some("resp-codex")).await;
    assert!(
        path.exists(),
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
    fixture.state.pending_terminals.stop_for_test().await;
    sqlx::query("DROP TRIGGER fail_operational_terminal")
        .execute(&pool)
        .await
        .unwrap();
    fixture.state.pending_terminals = crate::proxy_lifecycle::pending_terminal::Journal::new(
        directory,
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
        while path.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_exactly_once_side_effects(&fixture, request_id, Some("resp-codex")).await;
    pool.close().await;
    upstream.verify().await;
}

#[tokio::test]
async fn unknown_failure_retries_operational_sql_after_financial_arbitration_without_replay() {
    let fixture = codex_route_fixture("pending-unknown-operational-retry").await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_unknown_operational BEFORE INSERT ON proxy_operational_terminals \
         BEGIN SELECT RAISE(ABORT, 'UNKNOWN_OPERATIONAL_PRIVATE_CANARY'); END",
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
    let (path, original) = wait_for_receipt(&fixture).await;
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
    assert_eq!(receipt, original);
    assert_eq!(
        serde_json::from_slice::<Value>(&tokio::fs::read(path).await.unwrap()).unwrap(),
        original
    );
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
    let path = fixture
        .state
        .pending_terminals
        .directory_for_test()
        .join(format!("{}.json", original.0));
    assert!(
        !path.exists(),
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
    assert!(!path.exists());
    drop(held);
    let receipt =
        cancellation::assert_single_unknown_receipt(&fixture, 499, Some("client_cancelled")).await;
    assert_eq!(receipt["receipt_id"], original.0);
    assert_eq!(receipt["operational_terminal"]["completed_at"], original.2);
    assert!(path.exists(), "runtime ACK is not a financial receipt ACK");
    release.send(()).unwrap();
    server.await.unwrap();
    pool.close().await;
}
