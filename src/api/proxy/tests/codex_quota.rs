use super::*;

#[tokio::test]
async fn codex_quota_exhaustion_fails_over_once_and_future_request_skips_until_reset() {
    let fixture = codex_route_fixture("quota-reset-failover").await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-123"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": {"type":"usage_limit_reached", "resets_in_seconds":3600,
                "message":"fixture private exhausted quota"}
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-456"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("quota standby fixture").into_bytes(),
            "text/event-stream",
        ))
        .expect(2)
        .mount(&upstream)
        .await;
    let standby =
        add_codex_standby_route(&fixture, "codex-route-quota-reset-failover", "account-456").await;
    for _ in 0..2 {
        let response = send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model":fixture.model,"input":"fixture","stream":true}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("fixture private exhausted quota"));
    }
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let row = sqlx::query("SELECT last_failure_kind, cooldown_until-updated_at AS remaining FROM upstream_account_health WHERE upstream_account_id = $1")
        .bind(fixture.upstream_account_id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(row.get::<String, _>("last_failure_kind"), "quota_exhausted");
    assert!((3_590_000..=3_600_000).contains(&row.get::<i64, _>("remaining")));
    let actual: String = sqlx::query_scalar("SELECT upstream_account_id FROM request_records WHERE key_id = $1 ORDER BY created_at DESC LIMIT 1")
        .bind(fixture.key_id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(actual, standby.to_string());
    pool.close().await;
    upstream.verify().await;
}

#[tokio::test]
async fn codex_sse_quota_error_with_unknown_consumption_is_never_replayed() {
    let fixture = codex_route_fixture("quota-sse-unknown").await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-123"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            concat!(
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"quota-unknown\"}}\n\n",
                "data: {\"type\":\"response.failed\",\"response\":{\"id\":\"quota-unknown\",\"error\":{\"type\":\"usage_limit_reached\",\"message\":\"fixture private quota\"}}}\n\n"
            ), "text/event-stream"))
        .expect(1).mount(&upstream).await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-456"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("must not replay").into_bytes(),
            "text/event-stream",
        ))
        .expect(0)
        .mount(&upstream)
        .await;
    add_codex_standby_route(&fixture, "codex-route-quota-sse-unknown", "account-456").await;
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model":fixture.model,"input":"fixture","stream":true}),
    )
    .await;
    if let Ok(body) = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY).await {
        assert!(!String::from_utf8_lossy(&body).contains("must not replay"));
    }
    upstream.verify().await;
}
