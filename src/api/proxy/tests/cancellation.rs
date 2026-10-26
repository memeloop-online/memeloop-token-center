use super::*;

pub(super) async fn assert_unknown_request(
    fixture: &CodexRouteFixture,
    request_id: Uuid,
    active_owners: usize,
) {
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let row = sqlx::query(
                "SELECT q.completed_at, q.status_code, r.status AS reservation_status, r.actual_micros, \
                 p.state, p.unknown_reason, p.receipt_id, p.receipt_digest, p.cost_micros, \
                 o.status_code AS operational_status, o.error_code AS operational_error, \
                 o.completed_at AS operational_completed, o.stage AS operational_stage, \
                 (SELECT COUNT(*) FROM ledger_entries l WHERE l.source = q.reservation_id) AS ledger_count, \
                 (SELECT COUNT(*) FROM request_events e WHERE e.request_id = q.id AND e.event_kind = 'finished') AS event_count, \
                 (SELECT COUNT(*) FROM request_stats_facts f WHERE f.request_id = q.id) AS fact_count, \
                 (SELECT COUNT(*) FROM conversation_observations o WHERE o.request_id = q.id) AS observation_count \
                 FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id \
                 JOIN pending_proxy_settlements p ON p.request_id = q.id \
                 LEFT JOIN proxy_operational_terminals o ON o.request_id = q.id \
                 WHERE q.id = $1 AND q.key_id = $2",
            )
            .bind(request_id.to_string())
            .bind(fixture.key_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
            if row.get::<Option<String>, _>("unknown_reason").as_deref()
                == Some("authoritative_usage_unavailable")
                && fixture.state.proxy_lifecycles.active() == active_owners
                && row.get::<Option<i64>, _>("operational_completed").is_some()
            {
                assert_eq!(row.get::<Option<i64>, _>("completed_at"), None);
                assert_eq!(row.get::<Option<i64>, _>("status_code"), None);
                assert_eq!(row.get::<String, _>("reservation_status"), "reserved");
                assert_eq!(row.get::<Option<i64>, _>("actual_micros"), None);
                assert_eq!(row.get::<String, _>("state"), "pending");
                assert_eq!(row.get::<Option<String>, _>("receipt_id"), None);
                assert_eq!(row.get::<Option<String>, _>("receipt_digest"), None);
                assert_eq!(row.get::<Option<i64>, _>("cost_micros"), None);
                for field in ["ledger_count", "event_count", "fact_count", "observation_count"] {
                    assert_eq!(row.get::<i64, _>(field), 0, "{field}");
                }
                let records = fixture.state.db.list_requests(fixture.key_id, 10).await.unwrap();
                let record = records.iter().find(|record| record.request_id == request_id).unwrap();
                assert_eq!(record.status_code, row.get::<Option<i64>, _>("operational_status"));
                assert_eq!(record.error_code, row.get::<Option<String>, _>("operational_error"));
                assert_eq!(record.completed_at, None);
                assert_eq!(record.billing.cost, None);
                assert!(record.usage.tokens.is_none());
                assert_eq!(
                    record.lifecycle_state,
                    if record.status_code == Some(499) {
                        crate::model::RequestLifecycleState::Cancelled
                    } else if record.status_code.is_some_and(|status| status >= 400)
                        || record.error_code.is_some()
                    {
                        crate::model::RequestLifecycleState::Failed
                    } else {
                        crate::model::RequestLifecycleState::Succeeded
                    }
                );
                let operational = record.operational_terminal.as_ref().unwrap();
                assert_eq!(Some(operational.completed_at), row.get::<Option<i64>, _>("operational_completed"));
                assert_eq!(operational.stage, row.get::<Option<String>, _>("operational_stage"));
                assert_eq!(operational.financial_state, crate::model::RequestFinancialState::Unknown);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("unknown responsibility must retain the hold without financial effects and release the owner");
    pool.close().await;
}

pub(super) async fn assert_unknown_receipt(
    fixture: &CodexRouteFixture,
    request_id: Uuid,
    status_code: i64,
    error_code: &str,
    active_owners: usize,
) -> Value {
    let receipt = read_unknown_receipt(fixture, request_id, active_owners).await;
    assert_eq!(receipt["status_code"], status_code);
    assert_eq!(receipt["error_code"], error_code);
    receipt
}

pub(super) async fn single_unknown_receipt(fixture: &CodexRouteFixture) -> Value {
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    read_unknown_receipt(fixture, rows[0].request_id, 0).await
}

pub(super) async fn assert_single_unknown_receipt(
    fixture: &CodexRouteFixture,
    status_code: i64,
    error_code: Option<&str>,
) -> Value {
    let receipt = single_unknown_receipt(fixture).await;
    assert_eq!(receipt["status_code"], status_code);
    assert_eq!(receipt["error_code"], json!(error_code));
    receipt
}

async fn read_unknown_receipt(
    fixture: &CodexRouteFixture,
    request_id: Uuid,
    active_owners: usize,
) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
      assert_unknown_request(fixture, request_id, active_owners).await;
      loop {
        if let Some(receipt) = fixture
            .state
            .pending_terminals
            .receipt_jsons_for_test()
            .into_iter()
            .find(|receipt| receipt["request_id"] == request_id.to_string())
        {
        assert_eq!(receipt["usage_basis"], "not_observed");
        assert_eq!(receipt["claim"]["request_id"], receipt["request_id"]);
        assert_eq!(receipt["claim"]["tenant_id"], receipt["tenant_id"]);
        assert_eq!(receipt["claim"]["key_id"], fixture.key_id.to_string());
        assert_eq!(
            receipt["claim"]["reservation_id"],
            receipt["reservation"]["id"]
        );
        assert!(receipt["claim"]["fence"].as_str().is_some());
        assert!(
            receipt["claim"]["contract_digest"]
                .as_str()
                .is_some_and(|digest| !digest.is_empty())
        );
        let detail = fixture.state.db.request_archive_refs(fixture.key_id, request_id)
            .await.unwrap();
        let operational = detail.view.operational_terminal.as_ref().unwrap();
        assert_eq!(json!(detail.view.status_code), receipt["operational_terminal"]["status_code"]);
        assert_eq!(json!(detail.view.error_code), receipt["operational_terminal"]["error_code"]);
        assert_eq!(json!(detail.view.terminal_cause_code), receipt["operational_terminal"]["terminal_cause"]);
        assert_eq!(json!(operational.completed_at), receipt["operational_terminal"]["completed_at"]);
        assert_eq!(json!(operational.stage), receipt["operational_terminal"]["stage"]);
        return receipt;
        }
        tokio::task::yield_now().await;
      }
    }).await.expect("unknown terminal evidence and visible runtime failure must converge within the original budget")
}

async fn cancel_before_headers(stream: bool) {
    let fixture = codex_route_fixture("cancel-before-headers").await;
    let held = fixture.state.pending_terminals.hold_writer_for_test().await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
        .expect(1)
        .mount(&upstream)
        .await;
    let available = fixture.state.proxy_lifecycles.active();
    let mut request = Box::pin(send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "synthetic cancellation", "stream": stream}),
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            tokio::select! {
                _ = &mut request => panic!("upstream must not return headers before cancellation"),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {
                    if upstream.received_requests().await.unwrap().len() == 1 {
                        break;
                    }
                }
            }
        }
    })
    .await
    .unwrap();
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status_code, None);
    let request_id = rows[0].request_id;
    assert_eq!(fixture.state.proxy_lifecycles.active(), available + 1);
    drop(request);
    assert_unknown_request(&fixture, request_id, available).await;
    if let Ok(mut entries) =
        tokio::fs::read_dir(fixture.state.pending_terminals.directory_for_test()).await
    {
        assert!(
            entries.next_entry().await.unwrap().is_none(),
            "runtime ACK must not require journal publication"
        );
    }
    drop(held);
    let receipt =
        assert_unknown_receipt(&fixture, request_id, 499, "client_cancelled", available).await;
    assert_eq!(
        receipt["operational_terminal"]["stage"],
        "upstream_response_headers"
    );
    assert_eq!(receipt["terminal_cause"], "client_disconnected");
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    upstream.verify().await;
}

#[tokio::test]
async fn cancelled_stream_request_preserves_unknown_without_zero_settlement() {
    cancel_before_headers(true).await;
}

#[tokio::test]
async fn cancelled_buffered_request_preserves_unknown_without_zero_settlement() {
    cancel_before_headers(false).await;
}

#[tokio::test]
async fn streaming_handoff_keeps_the_stream_owner_and_successful_settlement() {
    let fixture = codex_route_fixture("cancel-guard-stream-handoff").await;
    let (endpoint, release, server) = gated_completed_sse_upstream_endpoint(
        String::new(),
        completed_codex_sse("synthetic handoff"),
    )
    .await;
    let response = send_codex_route_to_endpoint(
        &fixture,
        endpoint,
        "/v1/responses",
        json!({"model": fixture.model, "input": "synthetic handoff", "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status_code, None);
    release.send(()).unwrap();
    to_bytes(response.into_body(), usize::MAX).await.unwrap();
    server.await.unwrap();
    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(
        rows[0].status_code,
        Some(200),
        "terminal error_code={:?}",
        rows[0].error_code
    );
    assert_eq!(rows[0].error_code, None);
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-codex")).await;
}

#[tokio::test]
async fn malformed_handoff_prefix_keeps_the_incomplete_response_cause() {
    let fixture = codex_route_fixture("cancel-guard-malformed-prefix").await;
    let active_owners = fixture.state.proxy_lifecycles.active();
    let upstream = MockServer::start().await;
    let prefix = "event: response.created\ndata: {\"type\":\"response.created\"}\n\n";
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!("{prefix}{}", completed_codex_sse("synthetic handoff")),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "synthetic invalid prefix", "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let request_id =
        Uuid::parse_str(response.headers()[REQUEST_ID_HEADER].to_str().unwrap()).unwrap();
    let delivered = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let delivered = String::from_utf8(delivered.to_vec()).unwrap();
    assert_eq!(delivered.matches("event: response.failed").count(), 1);
    assert!(delivered.contains("upstream request failed"));
    assert!(!delivered.contains("response.completed"));
    assert!(!delivered.contains("synthetic handoff"));
    assert_unknown_receipt(
        &fixture,
        request_id,
        502,
        "upstream_incomplete_response",
        active_owners,
    )
    .await;
    upstream.verify().await;
}

#[tokio::test]
async fn cancellation_reloads_resized_reservation_and_settles_once() {
    let fixture = codex_route_fixture("cancel-resized-reservation").await;
    let database = &fixture.state.db;
    let key = database
        .authenticate_key(&fixture.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let price = database
        .upsert_model_price(&fixture.model, "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 7,
            output_token_ceiling: 11,
            protocol: "openai-responses",
            model: &fixture.model,
            request_object: &format!("gap://{request_id}/request"),
            upstream_account_id: Some(fixture.upstream_account_id),
            model_route_id: Some(fixture.route_id),
        })
        .await
        .unwrap();
    let resized = database
        .switch_pending_proxy_candidate(crate::db::SwitchProxyCandidateInput {
            request_id,
            tenant_id: key.tenant_id,
            key: &key,
            price: &price,
            reservation: &reservation,
            input_token_ceiling: 17,
            output_token_ceiling: 31,
            upstream_model: &fixture.upstream_model,
            output_token_limit: None,
            expected_assignment: (fixture.upstream_account_id, fixture.route_id),
            next_assignment: (fixture.upstream_account_id, fixture.route_id),
        })
        .await
        .unwrap();
    database
        .prepare_proxy_delivery(request_id, key.tenant_id, &resized, 17, 31, None)
        .await
        .unwrap();
    database
        .mark_proxy_delivery_started(request_id, key.tenant_id, &resized)
        .await
        .unwrap();
    let recovery_pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let durable: (i64, i64, String) = sqlx::query_as("SELECT q.input_tokens, q.output_tokens, r.price_snapshot_json FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id WHERE q.id = $1")
        .bind(request_id.to_string()).fetch_one(&recovery_pool).await.unwrap();
    assert_eq!((durable.0, durable.1), (17, 31));
    assert_eq!(
        serde_json::from_str::<Value>(&durable.2)
            .unwrap()
            .get("output_token_limit"),
        Some(&Value::Null)
    );
    recovery_pool.close().await;
    let recovered_database = crate::db::Database::connect(&fixture.database_url)
        .await
        .unwrap();
    assert!(matches!(
        recovered_database
            .finish_cancelled_proxy_request(request_id, key.tenant_id, &reservation, 100,)
            .await
            .unwrap(),
        FinishProxyRequestResult::Finished { .. }
    ));
    assert!(matches!(
        recovered_database
            .finish_cancelled_proxy_request(request_id, key.tenant_id, &reservation, 200,)
            .await
            .unwrap(),
        FinishProxyRequestResult::AlreadyFinished {
            status_code: 499,
            ..
        }
    ));
    assert!(matches!(
        recovered_database
            .expire_proxy_lifecycle_deadline(request_id, key.tenant_id, &resized, 300,)
            .await
            .unwrap(),
        FinishProxyRequestResult::AlreadyFinished {
            status_code: 499,
            ..
        }
    ));
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let row = sqlx::query(
        "SELECT q.error_code, q.duration_ms, q.cost_micros, r.status, r.actual_micros, (SELECT COUNT(*) FROM ledger_entries l WHERE l.source = q.reservation_id) AS ledger_count, (SELECT COUNT(*) FROM request_events e WHERE e.request_id = q.id AND e.event_kind = 'finished') AS event_count FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id WHERE q.id = $1",
    ).bind(request_id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(row.get::<String, _>("error_code"), "request_cancelled");
    assert_eq!(row.get::<i64, _>("duration_ms"), 100);
    assert_eq!(row.get::<i64, _>("cost_micros"), 0);
    assert_eq!(row.get::<String, _>("status"), "settled");
    assert_eq!(row.get::<i64, _>("actual_micros"), 0);
    assert_eq!(row.get::<i64, _>("ledger_count"), 1);
    assert_eq!(row.get::<i64, _>("event_count"), 1);
    pool.close().await;
}
