use super::*;

async fn make_account_half_open_probe(fixture: &CodexRouteFixture) {
    fixture
        .state
        .db
        .record_upstream_account_failure(
            fixture.upstream_account_id,
            1,
            UpstreamFailureKind::InvalidResponse,
        )
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

async fn wait_for_account_failure_count(fixture: &CodexRouteFixture, expected: i64) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
            let count: Option<i64> = sqlx::query_scalar(
                "SELECT consecutive_failures FROM upstream_account_health
                 WHERE upstream_account_id = $1",
            )
            .bind(fixture.upstream_account_id.to_string())
            .fetch_optional(&pool)
            .await
            .unwrap();
            pool.close().await;
            if count.unwrap_or_default() == expected {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
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
        "SELECT COUNT(*) FROM upstream_account_health WHERE upstream_account_id = $1",
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
async fn server_error_cools_the_account_and_fails_over_before_delivery() {
    let unavailable = MockServer::start().await;
    let healthy = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&unavailable)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(successful_chat_response())
        .expect(1)
        .mount(&healthy)
        .await;
    let fixture = resilient_route_fixture(
        "server-error-failover",
        &[(unavailable.uri(), 0), (healthy.uri(), 10)],
    )
    .await;

    let response = send_resilient_chat(&fixture, Some("server-error-failover"), false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    unavailable.verify().await;
    healthy.verify().await;

    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let actual: String = sqlx::query_scalar(
        "SELECT upstream_account_id FROM request_records WHERE key_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(fixture.key_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    let health: (i64, String) = sqlx::query_as(
        "SELECT consecutive_failures, last_failure_kind FROM upstream_account_health
         WHERE upstream_account_id = $1",
    )
    .bind(fixture.accounts[0].to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    assert_eq!(actual, fixture.accounts[1].to_string());
    assert_eq!(health, (1, "unavailable".to_owned()));
}

#[tokio::test]
async fn incomplete_sse_probe_stays_unhealthy_and_next_request_fails_over() {
    let fixture = codex_route_fixture("probe-incomplete-sse").await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-123"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-incomplete-probe\"}}\n\n",
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-456"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("standby after failed probe"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    add_codex_standby_route(&fixture, "codex-route-probe-incomplete-sse", "account-456").await;
    make_account_half_open_probe(&fixture).await;

    let first = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "failed probe", "stream": true}),
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let _ = to_bytes(first.into_body(), MAX_PROXY_RESPONSE_BODY).await;
    wait_for_request_settlement(&fixture, 1).await;
    wait_for_account_failure_count(&fixture, 2).await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let failure_kind: String = sqlx::query_scalar(
        "SELECT last_failure_kind FROM upstream_account_health WHERE upstream_account_id = $1",
    )
    .bind(fixture.upstream_account_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(failure_kind, "invalid_response");
    pool.close().await;

    let second = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "after failed probe", "stream": true}),
    )
    .await;
    assert_eq!(second.status(), StatusCode::OK);
    let body = to_bytes(second.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("standby after failed probe"));
    upstream.verify().await;
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
        if input == "valid probe" {
            wait_for_account_failure_count(&fixture, 0).await;
        }
    }
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let health_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM upstream_account_health WHERE upstream_account_id = $1",
    )
    .bind(fixture.upstream_account_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(health_rows, 0);
    pool.close().await;
    upstream.verify().await;
}
