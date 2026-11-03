use super::*;

#[tokio::test]
async fn preoutput_nonquota_sse_error_remains_failure_without_replay() {
    let fixture = codex_route_fixture("sse-auth-no-replay").await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header_matcher("chatgpt-account-id", "account-123"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"invalid_api_key\"}}}\n\n",
            "text/event-stream",
        )).expect(1).mount(&upstream).await;
    Mock::given(method("POST"))
        .and(header_matcher("chatgpt-account-id", "account-456"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(completed_codex_sse("must not replay"), "text/event-stream"),
        )
        .expect(0)
        .mount(&upstream)
        .await;
    add_codex_standby_route(&fixture, "codex-route-sse-auth-no-replay", "account-456").await;
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model":fixture.model,"input":"denied","stream":true}),
    )
    .await;
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("response.failed"), "{text}");
    assert!(!text.contains("must not replay"), "{text}");
    assert!(!text.contains("response.completed"), "{text}");
    fixture.state.routing_persistence.drain_for_test().await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upstream_supplier_availability")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    upstream.verify().await;
    pool.close().await;
}

#[tokio::test]
async fn chat_adapter_quota_is_classified_before_responses_translation() {
    let rejected = MockServer::start().await;
    let standby = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"error\":{\"code\":\"insufficient_quota\"}}\n\n",
            "text/event-stream",
        ))
        .expect(1)
        .mount(&rejected)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(completed_codex_sse("native standby"), "text/event-stream"),
        )
        .expect(1)
        .mount(&standby)
        .await;
    let fixture = resilient_route_fixture(
        "chat-adapter-quota",
        &[(rejected.uri(), 0), (standby.uri(), 10)],
    )
    .await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE upstream_accounts SET config_json=$2 WHERE id=$1")
        .bind(fixture.accounts[0].to_string())
        .bind(json!({"base_url":rejected.uri(),"network_scope":"public","responses_transport":"chat_completions"}).to_string())
        .execute(&pool).await.unwrap();
    let request = Request::post("/v1/responses")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {}", fixture.key))
        .body(Body::from(
            json!({"model":fixture.model,"input":"quota","stream":true}).to_string(),
        ))
        .unwrap();
    let response = router_for_role(fixture.state.clone(), RuntimeRole::Gateway)
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("native standby"), "{text}");
    assert!(!text.contains("response.failed"), "{text}");
    rejected.verify().await;
    standby.verify().await;
    pool.close().await;
}

#[tokio::test]
async fn delayed_sse_quota_body_after_headers_still_switches_to_complete_standby() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let upstream = axum::Router::new().fallback(|| async {
        let body = futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok::<_, std::io::Error>(Bytes::from_static(
                b"data: {\"error\":{\"code\":\"insufficient_quota\"}}\n\n",
            ))
        });
        (
            [(header::CONTENT_TYPE, "text/event-stream")],
            Body::from_stream(body),
        )
    });
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let standby = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(successful_chat_response())
        .expect(1)
        .mount(&standby)
        .await;
    let fixture =
        resilient_route_fixture("delayed-sse-quota", &[(endpoint, 0), (standby.uri(), 10)]).await;
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "ok");
    standby.verify().await;
    server.abort();
}

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
    let duplicate = fixture
        .state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: "resilient-quota-fourth-alias".into(),
            public_model: fixture.model.clone(),
            upstream_account_id: fixture.accounts[0],
            upstream_model: "upstream-0".into(),
            protocol: "openai".into(),
            priority: 5,
        })
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO routing_grants (tenant_id,key_id,model_route_id,route_group_id,created_at)
         SELECT tenant_id,id,$2,NULL,$3 FROM key_records WHERE id=$1",
    )
    .bind(fixture.key_id.to_string())
    .bind(duplicate.id.to_string())
    .bind(unix_millis())
    .execute(&pool)
    .await
    .unwrap();
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
