use super::*;

pub(super) async fn enable_provider_default_limits(fixture: &CodexRouteFixture) {
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let mut config: Value = serde_json::from_str(
        &sqlx::query_scalar::<_, String>("SELECT config_json FROM upstream_accounts WHERE id = $1")
            .bind(fixture.upstream_account_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap(),
    )
    .unwrap();
    config["transport_policy"] = json!({
        "chat_controls": "provider_default",
        "responses_output_limits": "provider_default"
    });
    sqlx::query("UPDATE upstream_accounts SET config_json = $1 WHERE id = $2")
        .bind(config.to_string())
        .bind(fixture.upstream_account_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn codex_default_limit_hints_preserve_trusted_reservation_and_observed_settlement() {
    for (protocol, stream, limit) in [
        ("responses", false, 16),
        ("responses", true, 1),
        ("chat", false, 1),
        ("chat", true, 16),
    ] {
        let label = format!("output-hint-{protocol}-{stream}");
        let fixture = codex_route_fixture(&label).await;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(codex_transport::RESPONSES_PATH))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(completed_codex_sse("synthetic answer"), "text/event-stream"),
            )
            .expect(1)
            .mount(&upstream)
            .await;
        let (path, body) = if protocol == "chat" {
            (
                "/v1/chat/completions",
                json!({"model":fixture.model,"messages":[{"role":"user","content":"synthetic"}],
                    "stream":stream,"max_tokens":limit}),
            )
        } else {
            (
                "/v1/responses",
                json!({"model":fixture.model,"input":"synthetic","stream":stream,
                    "max_output_tokens":limit}),
            )
        };
        let response = send_codex_route(&fixture, &upstream, path, body).await;
        assert_eq!(response.status(), StatusCode::OK);
        let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        assert!(!delivered.is_empty());
        wait_for_request_settlement(&fixture, 1).await;
        let records = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!((records[0].input_tokens, records[0].output_tokens), (3, 2));
        assert_ne!(records[0].cost, "0");
        assert_exactly_once_side_effects(
            &fixture,
            records[0].request_id,
            (protocol == "responses").then_some("resp-codex"),
        )
        .await;
        let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
        let reserved_tokens: i64 =
            sqlx::query_scalar("SELECT reserved_tokens FROM usage_reservations")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            reserved_tokens >= 64,
            "client hint must not shrink trusted reservation"
        );
        pool.close().await;
        let requests = upstream.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let wire: Value = serde_json::from_slice(&requests[0].body).unwrap();
        for field in ["max_output_tokens", "max_completion_tokens", "max_tokens"] {
            assert!(
                wire.get(field).is_none(),
                "{field} reached Codex OAuth upstream"
            );
        }
        upstream.verify().await;
    }
}

#[tokio::test]
async fn codex_output_limit_invalid_requests_have_no_admission_side_effects() {
    let fixture = codex_route_fixture("output-hint-invalid").await;
    enable_provider_default_limits(&fixture).await;
    let upstream = MockServer::start().await;
    for invalid in [
        Value::Null,
        json!(0),
        json!(-1),
        json!(1.5),
        json!("16"),
        json!({"max_output_tokens":16,"max_tokens":16}),
        json!({"reservation_token_bounds":{"fake":1}}),
    ] {
        let mut body = json!({"model":fixture.model,"input":"synthetic"});
        if let Some(fields) = invalid.as_object() {
            body.as_object_mut().unwrap().extend(fields.clone());
        } else {
            body["max_output_tokens"] = invalid;
        }
        let response = send_codex_route(&fixture, &upstream, "/v1/responses", body).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
    assert!(
        fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap()
            .is_empty()
    );
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let reservations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_reservations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(reservations, 0);
    pool.close().await;
}
