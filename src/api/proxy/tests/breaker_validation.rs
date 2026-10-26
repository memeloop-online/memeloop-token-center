use super::*;

mod delivered_probe;

async fn make_account_half_open_probe(fixture: &CodexRouteFixture) {
    make_account_half_open_probe_with_kind(fixture, UpstreamFailureKind::InvalidResponse).await;
}

async fn make_account_half_open_probe_with_kind(
    fixture: &CodexRouteFixture,
    kind: UpstreamFailureKind,
) {
    fixture
        .state
        .db
        .record_upstream_account_failure(fixture.upstream_account_id, 1, kind)
        .await
        .unwrap();
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query(
        "UPDATE upstream_account_health SET cooldown_until = 0, probe_lease_until = 0
         WHERE upstream_account_id = $1",
    )
    .bind(fixture.upstream_account_id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn ordinary_client_error_does_not_cool_down_a_shared_account() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"message": "caller input is invalid"}
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = resilient_route_fixture("ordinary-client-error", &[(upstream.uri(), 0)]).await;
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let health_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM upstream_account_health
         WHERE upstream_account_id = $1 AND consecutive_failures > 0",
    )
    .bind(fixture.accounts[0].to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(health_rows, 0);
    pool.close().await;
    upstream.verify().await;
}

#[tokio::test]
async fn codex_connect_failure_does_not_replay_and_keeps_transport_observation() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("next independent request"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = codex_route_fixture("connect-observation").await;
    let response = routing::with_test_pre_delivery_connect_failures(
        1,
        send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model": fixture.model, "input": "connect failure", "stream": false}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let request_id =
        Uuid::parse_str(response.headers()[REQUEST_ID_HEADER].to_str().unwrap()).unwrap();
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(upstream.received_requests().await.unwrap().is_empty());
    fixture.state.routing_persistence.drain_for_test().await;
    let diagnostics = fixture
        .state
        .db
        .request_upstream_transport_diagnostics(request_id)
        .await
        .unwrap();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(
        diagnostics[0].upstream_account_id,
        fixture.upstream_account_id
    );
    assert_eq!(diagnostics[0].failure_kind, "connection");
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let health_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM upstream_account_health
         WHERE upstream_account_id = $1 AND consecutive_failures > 0",
    )
    .bind(fixture.upstream_account_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    assert_eq!(
        health_rows, 0,
        "a customer connection failure is not an account health sample"
    );
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "next independent request", "stream": false}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("next independent request"));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let rows = fixture
                .state
                .db
                .list_requests(fixture.key_id, 10)
                .await
                .unwrap();
            if rows.len() == 2
                && rows
                    .iter()
                    .any(|row| row.status_code == Some(200) && row.completed_at.is_some())
            {
                let completed = rows
                    .iter()
                    .find(|row| row.status_code == Some(200))
                    .unwrap();
                assert_exactly_once_side_effects(
                    &fixture,
                    completed.request_id,
                    Some("resp-codex"),
                )
                .await;
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    upstream.verify().await;
}

#[tokio::test]
async fn ambiguous_codex_response_does_not_open_the_shared_account_breaker() {
    let (endpoint, upstream) = truncated_sse_upstream_endpoint(
        b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"private-secret\"}}",
    )
    .await;
    let fixture = codex_route_fixture("ambiguous-response-inconclusive").await;
    let response = send_codex_route_to_endpoint(
        &fixture,
        endpoint,
        "/v1/responses",
        json!({
            "model": fixture.model,
            "input": "ambiguous response must not poison the account",
            "stream": true
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let rendered = String::from_utf8(body.to_vec()).unwrap();
    assert_eq!(rendered.matches("event: response.failed").count(), 1);
    assert_eq!(rendered.matches("upstream request failed").count(), 1);
    assert!(!rendered.contains("private-secret"));
    cancellation::assert_single_unknown_receipt(&fixture, 502, Some("upstream_stream_read_error"))
        .await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status_code, Some(502));
    assert_eq!(
        rows[0].error_code.as_deref(),
        Some("upstream_stream_read_error")
    );
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let health_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM upstream_account_health
         WHERE upstream_account_id = $1 AND consecutive_failures > 0",
    )
    .bind(fixture.upstream_account_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        health_rows, 0,
        "a response that may already have executed is inconclusive account-health evidence"
    );
    pool.close().await;
    upstream.await.unwrap();
}

#[tokio::test]
async fn streaming_transport_terminal_is_cached_before_immediate_same_session_retry() {
    let (failed_endpoint, failed_upstream) = truncated_sse_upstream_endpoint(
        b"data: {\"id\":\"partial\",\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
    )
    .await;
    let healthy = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(successful_chat_response())
        .expect(1)
        .mount(&healthy)
        .await;
    let fixture = resilient_route_fixture(
        "stream-terminal-before-retry",
        &[(failed_endpoint, 0), (healthy.uri(), 10)],
    )
    .await;
    let session_id = "stream-terminal-before-retry";

    let first = send_resilient_chat(&fixture, Some(session_id), true).await;
    assert_eq!(first.status(), StatusCode::OK);
    let mut first_body = first.into_body().into_data_stream();
    let mut observed_disconnect = false;
    while let Some(chunk) = futures_util::StreamExt::next(&mut first_body).await {
        if chunk.is_err() {
            observed_disconnect = true;
        }
    }
    assert!(observed_disconnect);

    let second = send_resilient_chat(&fixture, Some(session_id), false).await;
    assert_eq!(second.status(), StatusCode::OK);
    let _ = to_bytes(second.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();

    failed_upstream.await.unwrap();
    healthy.verify().await;
}

#[tokio::test]
async fn failed_codex_protocol_mismatch_retains_invalid_response_observation() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            concat!(
                "event: response.failed\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-mismatch\",\"error\":null}}\n\n"
            ),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = codex_route_fixture("failed-protocol-mismatch").await;
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({
            "model": fixture.model,
            "input": "invalid failed terminal",
            "stream": true
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let rendered = String::from_utf8(body.to_vec()).unwrap();
    assert_eq!(rendered.matches("event: response.failed").count(), 1);
    assert_eq!(rendered.matches("upstream request failed").count(), 1);
    cancellation::assert_single_unknown_receipt(
        &fixture,
        502,
        Some("upstream_response_terminal_conflict"),
    )
    .await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status_code, Some(502));
    assert_eq!(
        rows[0].error_code.as_deref(),
        Some("upstream_response_terminal_conflict")
    );
    fixture.state.routing_persistence.drain_for_test().await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let health_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM upstream_account_health
         WHERE upstream_account_id = $1 AND consecutive_failures > 0",
    )
    .bind(fixture.upstream_account_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    assert_eq!(
        health_rows, 0,
        "a protocol mismatch is request evidence, not an account health sample"
    );
    upstream.verify().await;
}

#[tokio::test]
async fn server_error_is_preserved_then_cools_the_account_for_the_next_request() {
    let unavailable = MockServer::start().await;
    let healthy = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .expect(2)
        .mount(&unavailable)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(successful_chat_response())
        .expect(0)
        .mount(&healthy)
        .await;
    let fixture = resilient_route_fixture(
        "server-error-failover",
        &[(unavailable.uri(), 0), (healthy.uri(), 10)],
    )
    .await;

    let session_id = "server-error-same-session";
    let response = send_resilient_chat(&fixture, Some(session_id), false).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();

    // A 5xx is request evidence. It does not cool the account or move the
    // next request to the standby. Account health comes only from its probe.
    fixture.state.routing_persistence.drain_for_test().await;
    let response = send_resilient_chat(&fixture, Some(session_id), false).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    unavailable.verify().await;
    healthy.verify().await;

    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let health_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM upstream_account_health
         WHERE upstream_account_id = $1 AND consecutive_failures > 0",
    )
    .bind(fixture.accounts[0].to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    assert_eq!(health_rows, 0);
}

#[tokio::test]
async fn valid_settled_sse_probe_recovers_the_account() {
    let fixture = codex_route_fixture("probe-valid-sse").await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-123"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(completed_codex_sse("valid probe"), "text/event-stream"),
        )
        .expect(2)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(header_matcher("chatgpt-account-id", "account-456"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&upstream)
        .await;
    add_codex_standby_route(&fixture, "codex-route-probe-valid-sse", "account-456").await;
    make_account_half_open_probe(&fixture).await;

    for input in ["valid probe", "after recovery"] {
        let response = send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model": fixture.model, "input": input, "stream": true}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        wait_for_request_settlement(&fixture, if input == "valid probe" { 1 } else { 2 }).await;
    }
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let health_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM upstream_account_health
         WHERE upstream_account_id = $1 AND consecutive_failures > 0",
    )
    .bind(fixture.upstream_account_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        health_rows, 1,
        "a successful customer request does not clear account health"
    );
    pool.close().await;
    upstream.verify().await;
}
