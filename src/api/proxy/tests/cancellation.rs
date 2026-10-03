use super::*;

async fn cancel_before_headers(stream: bool) {
    let fixture = codex_route_fixture("cancel-before-headers").await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
        .expect(1)
        .mount(&upstream)
        .await;
    let available = fixture.state.proxy_lifecycle_permits.available_permits();
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
    assert_eq!(
        fixture.state.proxy_lifecycle_permits.available_permits(),
        available - 1
    );
    drop(request);
    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(499));
    assert_eq!(rows[0].error_code.as_deref(), Some("request_cancelled"));
    assert_eq!(rows[0].cost, "0");
    tokio::time::timeout(Duration::from_secs(3), async {
        while fixture.state.proxy_lifecycle_permits.available_permits() != available {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let row = sqlx::query(
        "SELECT r.status, r.actual_micros, (SELECT COUNT(*) FROM ledger_entries l WHERE l.source = q.reservation_id) AS ledger_count, (SELECT COUNT(*) FROM request_events e WHERE e.request_id = q.id AND e.event_kind = 'finished') AS event_count FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id WHERE q.id = $1",
    ).bind(request_id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(row.get::<String, _>("status"), "settled");
    assert_eq!(row.get::<i64, _>("actual_micros"), 0);
    assert_eq!(row.get::<i64, _>("ledger_count"), 1);
    assert_eq!(row.get::<i64, _>("event_count"), 1);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    pool.close().await;
}

#[tokio::test]
async fn cancelled_stream_request_settles_without_waiting_for_orphan_reaper() {
    cancel_before_headers(true).await;
}

#[tokio::test]
async fn cancelled_buffered_request_settles_without_waiting_for_orphan_reaper() {
    cancel_before_headers(false).await;
}

#[tokio::test]
async fn streaming_handoff_keeps_the_stream_owner_and_successful_settlement() {
    let fixture = codex_route_fixture("cancel-guard-stream-handoff").await;
    let prefix = "event: response.created\ndata: {\"type\":\"response.created\"}\n\n".to_owned();
    let (endpoint, release, server) =
        gated_completed_sse_upstream_endpoint(prefix, completed_codex_sse("synthetic handoff"))
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
    assert_eq!(rows[0].status_code, Some(200));
    assert_eq!(rows[0].error_code, None);
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-codex")).await;
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
    assert!(matches!(
        database
            .finish_cancelled_proxy_request(request_id, key.tenant_id, &reservation, 100,)
            .await
            .unwrap(),
        FinishProxyRequestResult::Finished { .. }
    ));
    assert!(matches!(
        database
            .finish_cancelled_proxy_request(request_id, key.tenant_id, &reservation, 200,)
            .await
            .unwrap(),
        FinishProxyRequestResult::AlreadyFinished {
            status_code: 499,
            ..
        }
    ));
    assert!(matches!(
        database
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
