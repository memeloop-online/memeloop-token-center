use super::*;

async fn set_policy(fixture: &CodexRouteFixture, policy: Value) {
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let encoded: String =
        sqlx::query_scalar("SELECT config_json FROM upstream_accounts WHERE id = $1")
            .bind(fixture.upstream_account_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    let mut config: Value = serde_json::from_str(&encoded).unwrap();
    config["transport_policy"] = policy;
    sqlx::query("UPDATE upstream_accounts SET config_json = $1 WHERE id = $2")
        .bind(config.to_string())
        .bind(fixture.upstream_account_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn runtime_candidate_budget_controls_quota_failover_without_reloading_service() {
    for limit in [1, 2] {
        let label = format!("policy-budget-{limit}");
        let fixture = codex_route_fixture(&label).await;
        set_policy(&fixture, json!({"version": 1, "candidate_attempts": limit})).await;
        add_codex_standby_route(&fixture, &format!("codex-route-{label}"), "account-456").await;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(codex_transport::RESPONSES_PATH))
            .and(header_matcher("chatgpt-account-id", "account-123"))
            .respond_with(ResponseTemplate::new(429).set_body_json(json!({
                "error": {"type": "usage_limit_reached", "resets_in_seconds": 3600}
            })))
            .expect(1)
            .mount(&upstream)
            .await;
        Mock::given(method("POST"))
            .and(path(codex_transport::RESPONSES_PATH))
            .and(header_matcher("chatgpt-account-id", "account-456"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                completed_codex_sse("bounded standby").into_bytes(),
                "text/event-stream",
            ))
            .expect(if limit == 1 { 0 } else { 1 })
            .mount(&upstream)
            .await;
        let response = send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model": fixture.model, "input": "fixture", "stream": false}),
        )
        .await;
        assert_eq!(
            response.status(),
            if limit == 1 {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::OK
            }
        );
        let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        upstream.verify().await;
    }
}

#[tokio::test]
async fn codex_dispatched_503_never_replays_despite_an_available_budget_and_standby() {
    let fixture = codex_route_fixture("policy-503").await;
    set_policy(&fixture, json!({"version": 1, "candidate_attempts": 8})).await;
    add_codex_standby_route(&fixture, "codex-route-policy-503", "account-456").await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-123"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({"error": "unavailable"})))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-456"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "fixture", "stream": false}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    upstream.verify().await;
}
