use super::*;

#[tokio::test]
async fn preoutput_sse_quota_and_rate_rejection_fail_over_with_one_complete_response() {
    for (label, rejection) in [
        (
            "failed",
            "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp-rejected\",\"status\":\"failed\",\"error\":{\"code\":\"insufficient_quota\"}}}\n\n",
        ),
        (
            "error",
            "event: error\ndata: {\"type\":\"error\",\"code\":\"rate_limit_exceeded\"}\n\n",
        ),
        (
            "nested",
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"code\":\"insufficient_user_quota\"}}\n\n",
        ),
    ] {
        let name = format!("stream-quota-{label}");
        let fixture = codex_route_fixture(&name).await;
        let upstream = MockServer::start().await;
        let prefix = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-rejected\",\"output\":[],\"usage\":null}}\n\n";
        Mock::given(method("POST"))
            .and(header_matcher("chatgpt-account-id", "account-123"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("retry-after", "120")
                    .set_body_raw(format!("{prefix}{rejection}"), "text/event-stream"),
            )
            .expect(1)
            .mount(&upstream)
            .await;
        Mock::given(method("POST"))
            .and(header_matcher("chatgpt-account-id", "account-456"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(completed_codex_sse("standby complete"), "text/event-stream"),
            )
            .expect(2)
            .mount(&upstream)
            .await;
        let standby =
            add_codex_standby_route(&fixture, &format!("codex-route-{name}"), "account-456").await;
        for count in 1..=2 {
            let response = send_codex_route(
                &fixture,
                &upstream,
                "/v1/responses",
                json!({"model":fixture.model,"input":"quota failover","stream":true}),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
                .await
                .unwrap();
            let text = String::from_utf8_lossy(&body);
            assert!(text.contains("standby complete"), "{text}");
            assert!(!text.contains("resp-rejected"), "{text}");
            assert!(!text.contains("response.failed"), "{text}");
            assert_eq!(text.matches("event: response.completed").count(), 1);
            wait_for_request_settlement(&fixture, count).await;
            fixture.state.routing_persistence.drain_for_test().await;
        }
        let rows = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        for row in rows {
            assert_eq!(row.upstream_account_id, Some(standby));
            assert_eq!((row.input_tokens, row.output_tokens), (3, 2));
            assert_eq!(row.error_code, None);
        }
        let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM ledger_entries l JOIN request_records r ON l.source=r.reservation_id WHERE r.key_id=$1",
        ).bind(fixture.key_id.to_string()).fetch_one(&pool).await.unwrap();
        assert_eq!(count, 2);
        let failed_model: String = sqlx::query_scalar(
            "SELECT upstream_model FROM upstream_supplier_availability WHERE upstream_account_id=$1",
        ).bind(fixture.upstream_account_id.to_string()).fetch_one(&pool).await.unwrap();
        assert_eq!(failed_model, fixture.upstream_model);
        upstream.verify().await;
        pool.close().await;
    }
}

#[tokio::test]
async fn definitive_rejections_reach_fourth_authorized_model_alias_candidate() {
    let rejected = MockServer::start().await;
    let healthy = MockServer::start().await;
    let unauthorized = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "120")
                .set_body_json(json!({"error":{"code":"insufficient_quota"}})),
        )
        .expect(3)
        .mount(&rejected)
        .await;
    Mock::given(method("POST"))
        .respond_with(successful_chat_response())
        .expect(1)
        .mount(&healthy)
        .await;
    Mock::given(method("POST"))
        .respond_with(successful_chat_response())
        .expect(0)
        .mount(&unauthorized)
        .await;
    let fixture = resilient_route_fixture(
        "quota-fourth-alias",
        &[
            (rejected.uri(), 0),
            (rejected.uri(), 10),
            (rejected.uri(), 20),
            (healthy.uri(), 30),
            (unauthorized.uri(), -10),
        ],
    )
    .await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("DELETE FROM routing_grants WHERE key_id=$1 AND model_route_id IN (SELECT id FROM model_routes WHERE upstream_account_id=$2)")
        .bind(fixture.key_id.to_string()).bind(fixture.accounts[4].to_string())
        .execute(&pool).await.unwrap();
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "ok");
    let requests = healthy.received_requests().await.unwrap();
    let forwarded: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(forwarded["model"], "upstream-3");
    rejected.verify().await;
    healthy.verify().await;
    unauthorized.verify().await;
    pool.close().await;
}
