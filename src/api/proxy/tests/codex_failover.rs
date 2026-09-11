use super::*;
use crate::db::UpdateUpstreamAccountInput;

async fn set_route_priority(fixture: &CodexRouteFixture, account_id: Uuid, priority: i64) {
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query(
        "UPDATE model_routes SET priority = $1
         WHERE upstream_account_id = $2 AND public_model = $3",
    )
    .bind(priority)
    .bind(account_id.to_string())
    .bind(&fixture.model)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}

async fn selected_account(fixture: &CodexRouteFixture) -> Uuid {
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let account: String = sqlx::query_scalar(
        "SELECT upstream_account_id FROM request_records
         WHERE key_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(fixture.key_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    Uuid::parse_str(&account).unwrap()
}

async fn failure_kind(fixture: &CodexRouteFixture, account_id: Uuid) -> String {
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let kind = sqlx::query_scalar(
        "SELECT last_failure_kind FROM upstream_account_health
         WHERE upstream_account_id = $1",
    )
    .bind(account_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    kind
}

#[tokio::test]
async fn codex_503_fails_over_before_downstream_delivery() {
    let fixture = codex_route_fixture("503-failover").await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-123"))
        .respond_with(
            ResponseTemplate::new(503)
                .set_body_string("private primary 503 body must not reach the client"),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-456"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("healthy standby after 503"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let standby = add_codex_standby_route(
        &fixture,
        "codex-route-503-failover",
        "account-456",
    )
    .await;

    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "fail over", "stream": false}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("healthy standby after 503"));
    assert!(!body.contains("private primary"));
    wait_for_request_settlement(&fixture, 1).await;
    assert_eq!(selected_account(&fixture).await, standby);
    assert_eq!(
        failure_kind(&fixture, fixture.upstream_account_id).await,
        "unavailable"
    );
    upstream.verify().await;
}

#[tokio::test]
async fn account_policy_can_disable_codex_503_failover_at_runtime() {
    let fixture = codex_route_fixture("503-policy-disabled").await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-123"))
        .respond_with(ResponseTemplate::new(503).set_body_string("private disabled-policy body"))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(header_matcher("chatgpt-account-id", "account-456"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&upstream)
        .await;
    add_codex_standby_route(
        &fixture,
        "codex-route-503-policy-disabled",
        "account-456",
    )
    .await;
    let (account, _) = fixture
        .state
        .db
        .upstream_account_with_credential(
            fixture.upstream_account_id,
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let mut config = account.config.clone();
    config["transport_policy"] = json!({"service_unavailable_failover": false});
    fixture
        .state
        .db
        .update_upstream_account(
            account.id,
            "codex-route-503-policy-disabled",
            UpdateUpstreamAccountInput {
                name: account.name,
                config,
                expected_updated_at: account.updated_at,
            },
        )
        .await
        .unwrap();

    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "honor policy", "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&body).contains("private disabled-policy"));
    wait_for_request_settlement(&fixture, 1).await;
    assert_eq!(
        selected_account(&fixture).await,
        fixture.upstream_account_id
    );
    upstream.verify().await;
}

#[tokio::test]
async fn all_codex_503_candidates_stop_at_the_global_attempt_budget() {
    let fixture = codex_route_fixture("all-503-budget").await;
    let upstream = MockServer::start().await;
    let first = fixture.upstream_account_id;
    let second = add_codex_standby_route(
        &fixture,
        "codex-route-all-503-budget",
        "account-456",
    )
    .await;
    let third = add_codex_standby_route(
        &fixture,
        "codex-route-all-503-budget",
        "account-789",
    )
    .await;
    set_route_priority(&fixture, third, 20).await;
    for account in ["account-123", "account-456", "account-789"] {
        Mock::given(method("POST"))
            .and(path(codex_transport::RESPONSES_PATH))
            .and(header_matcher("chatgpt-account-id", account))
            .respond_with(
                ResponseTemplate::new(503)
                    .set_body_string(format!("private 503 response from {account}")),
            )
            .expect(1)
            .mount(&upstream)
            .await;
    }

    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "bounded", "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&body).contains("private 503"));
    wait_for_request_settlement(&fixture, 1).await;
    assert_eq!(selected_account(&fixture).await, third);
    for account in [first, second, third] {
        assert_eq!(failure_kind(&fixture, account).await, "unavailable");
    }
    upstream.verify().await;
}

#[tokio::test]
async fn connection_then_quota_exhaustion_still_reaches_the_third_account() {
    let fixture = codex_route_fixture("connection-quota-budget").await;
    let upstream = MockServer::start().await;
    let exhausted = add_codex_standby_route(
        &fixture,
        "codex-route-connection-quota-budget",
        "account-456",
    )
    .await;
    let healthy = add_codex_standby_route(
        &fixture,
        "codex-route-connection-quota-budget",
        "account-789",
    )
    .await;
    set_route_priority(&fixture, healthy, 20).await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-123"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-456"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": {
                "type": "usage_limit_reached",
                "resets_in_seconds": 3600,
                "message": "private quota detail"
            }
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-789"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("third account healthy"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;

    let response = routing::with_test_pre_delivery_connect_failures(
        2,
        send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model": fixture.model, "input": "three accounts", "stream": false}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("third account healthy"));
    assert!(!body.contains("private quota detail"));
    wait_for_request_settlement(&fixture, 1).await;
    assert_eq!(selected_account(&fixture).await, healthy);
    assert_eq!(
        failure_kind(&fixture, fixture.upstream_account_id).await,
        "connection"
    );
    assert_eq!(failure_kind(&fixture, exhausted).await, "quota_exhausted");
    upstream.verify().await;
}

#[tokio::test]
async fn concurrent_503_wave_never_exposes_the_bad_account_to_callers() {
    let fixture = codex_route_fixture("503-concurrent-wave").await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-123"))
        .respond_with(ResponseTemplate::new(503).set_body_string("private concurrent 503"))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-456"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("concurrent standby"),
            "text/event-stream",
        ))
        .expect(4)
        .mount(&upstream)
        .await;
    add_codex_standby_route(
        &fixture,
        "codex-route-503-concurrent-wave",
        "account-456",
    )
    .await;

    let responses = futures_util::future::join_all((0..4).map(|ordinal| {
        send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({
                "model": fixture.model,
                "input": format!("concurrent-{ordinal}"),
                "stream": false
            }),
        )
    }))
    .await;
    for response in responses {
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("concurrent standby"));
        assert!(!body.contains("private concurrent 503"));
    }
    let requests = upstream.received_requests().await.unwrap();
    let primary_requests = requests
        .iter()
        .filter(|request| {
            request
                .headers
                .get("chatgpt-account-id")
                .and_then(|value| value.to_str().ok())
                == Some("account-123")
        })
        .count();
    assert!((1..=4).contains(&primary_requests));
    upstream.verify().await;
}
