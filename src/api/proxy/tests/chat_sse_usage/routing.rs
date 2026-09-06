use super::support::*;
use super::*;

#[tokio::test]
async fn non_opt_in_chat_routes_transparently_forward_n() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({"n": 2})))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(
                [
                    chat_content("chatcmpl-n-two"),
                    chat_finish("chatcmpl-n-two"),
                    done().to_owned(),
                ]
                .concat(),
                "text/event-stream",
            ),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture =
        response_usage_fixture_with_contract("chat-n-transparent", &upstream, 0, None).await;
    let mut request = chat_request(&fixture.model);
    request["n"] = json!(2);
    let response = send_chat_usage_request(&fixture, &request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("\"content\":\"ok\""));
    upstream.verify().await;
    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(200));
    assert_eq!(rows[0].output_tokens, 32);
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
}

#[tokio::test]
async fn non_opt_in_chat_n_output_reservation_overflow_is_rejected_before_admission() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;
    let fixture =
        response_usage_fixture_with_contract("chat-n-reservation-overflow", &upstream, 0, None)
            .await;
    let mut request = chat_request(&fixture.model);
    request["n"] = json!(MAX_REPORTED_TOKENS);
    let response = send_chat_usage_request(&fixture, &request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap()
            .is_empty()
    );
    upstream.verify().await;
}

async fn add_http_chat_standby(
    fixture: &CodexRouteFixture,
    label: &str,
    upstream: &MockServer,
    stream_usage_contract: Option<&str>,
    priority: i64,
) {
    let tenant = format!("compatibility-route-{label}");
    let mut config = json!({
        "base_url": upstream.uri(),
        "network_scope": "public",
    });
    if let Some(stream_usage_contract) = stream_usage_contract {
        config["stream_usage_contract"] = json!(stream_usage_contract);
    }
    let account = fixture
        .state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.clone(),
                name: format!("compatibility-chat-standby-{label}-{priority}"),
                driver: "http-json".to_owned(),
                config,
                credential: UpstreamCredential::ApiKey {
                    value: "compatibility-standby-secret".to_owned(),
                    header: "authorization".to_owned(),
                    prefix: "Bearer ".to_owned(),
                },
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let route = fixture
        .state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant,
            public_model: fixture.model.clone(),
            upstream_account_id: account.id,
            upstream_model: fixture.model.clone(),
            protocol: "openai".to_owned(),
            priority,
        })
        .await
        .unwrap();
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let tenant_id: String = sqlx::query_scalar("SELECT tenant_id FROM key_records WHERE id = $1")
        .bind(fixture.key_id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO routing_grants (tenant_id, key_id, model_route_id, route_group_id, created_at) \
         VALUES ($1, $2, $3, NULL, $4)",
    )
    .bind(tenant_id)
    .bind(fixture.key_id.to_string())
    .bind(route.id.to_string())
    .bind(crate::db::unix_millis())
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn mixed_chat_candidates_skip_strict_usage_route_for_n_two() {
    let strict_upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&strict_upstream)
        .await;
    let standby_upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({"n": 2})))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(
                [
                    chat_content("chatcmpl-mixed-n-two"),
                    chat_finish("chatcmpl-mixed-n-two"),
                    done().to_owned(),
                ]
                .concat(),
                "text/event-stream",
            ),
        )
        .expect(1)
        .mount(&standby_upstream)
        .await;
    let label = "chat-mixed-n-two";
    let fixture = response_usage_fixture(label, &strict_upstream, 0).await;
    // Populate four request-local incompatible candidates, then prove a fifth
    // compatible route remains discoverable without consuming the three-send
    // attempt budget.
    add_http_chat_standby(
        &fixture,
        label,
        &strict_upstream,
        Some("openai-chat-usage-only"),
        10,
    )
    .await;
    add_http_chat_standby(
        &fixture,
        label,
        &strict_upstream,
        Some("openai-chat-usage-only"),
        20,
    )
    .await;
    add_http_chat_standby(
        &fixture,
        label,
        &strict_upstream,
        Some("openai-chat-usage-only"),
        30,
    )
    .await;
    add_http_chat_standby(&fixture, label, &standby_upstream, None, 40).await;
    let mut request = chat_request(&fixture.model);
    request["n"] = json!(2);
    let response = send_chat_usage_request(&fixture, &request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("\"content\":\"ok\""));
    strict_upstream.verify().await;
    standby_upstream.verify().await;
}

#[tokio::test]
async fn chat_n_greater_than_one_is_rejected_before_upstream_or_reservation() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;
    let fixture = response_usage_fixture("chat-n-rejected", &upstream, 0).await;
    let mut request = chat_request(&fixture.model);
    request["n"] = json!(2);
    let response = send_chat_usage_request(&fixture, &request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap()
            .is_empty()
    );
    upstream.verify().await;
}
