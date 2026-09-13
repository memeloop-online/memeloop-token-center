use super::*;

async fn policy_api(
    fixture: &CodexRouteFixture,
    token: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let response = router_for_role(fixture.state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn set_policy(fixture: &CodexRouteFixture, policy: Value) {
    let tenant = fixture.model.replacen("codex-public-", "codex-route-", 1);
    let service = fixture
        .state
        .db
        .create_service_token(
            crate::db::CreateServiceTokenInput {
                name: "policy-manager".into(),
                scopes: vec!["providers:read".into(), "providers:write".into()],
                tenant_external_id: Some(tenant.clone()),
            },
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let list_uri = format!("/internal/v1/upstreams?tenant_external_id={tenant}");
    let (status, accounts) = policy_api(fixture, &service.token, "GET", &list_uri, None).await;
    assert_eq!(status, StatusCode::OK);
    let original = accounts
        .as_array()
        .unwrap()
        .iter()
        .find(|account| account["id"] == fixture.upstream_account_id.to_string())
        .unwrap();
    let mut config = original["config"].clone();
    config["transport_policy"] = policy.clone();
    let update = json!({"tenant_external_id": tenant, "name": original["name"],
        "config": config, "expected_updated_at": original["updated_at"]});
    let uri = format!("/internal/v1/upstreams/{}", fixture.upstream_account_id);
    let (status, saved) =
        policy_api(fixture, &service.token, "PUT", &uri, Some(update.clone())).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["config"]["transport_policy"], policy);
    assert!(saved["updated_at"].as_i64().unwrap() > original["updated_at"].as_i64().unwrap());

    let mut stale = update.clone();
    stale["config"] = original["config"].clone();
    assert_eq!(
        policy_api(fixture, &service.token, "PUT", &uri, Some(stale))
            .await
            .0,
        StatusCode::CONFLICT
    );
    let mut wrong_tenant = update.clone();
    wrong_tenant["tenant_external_id"] = json!("another-policy-tenant");
    assert_eq!(
        policy_api(fixture, &service.token, "PUT", &uri, Some(wrong_tenant))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    for invalid in [
        json!({"version": 2}),
        json!({"candidate_attempts": 9}),
        json!({"failover_deadline_millis": 999}),
        json!({"retry_503": true}),
    ] {
        let mut rejected = update.clone();
        rejected["expected_updated_at"] = saved["updated_at"].clone();
        rejected["config"]["transport_policy"] = invalid;
        assert_eq!(
            policy_api(fixture, &service.token, "PUT", &uri, Some(rejected))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    let (status, accounts) = policy_api(fixture, &service.token, "GET", &list_uri, None).await;
    assert_eq!(status, StatusCode::OK);
    let read_back = accounts
        .as_array()
        .unwrap()
        .iter()
        .find(|account| account["id"] == fixture.upstream_account_id.to_string())
        .unwrap();
    assert_eq!(read_back["config"]["transport_policy"], policy);
    assert_eq!(read_back["updated_at"], saved["updated_at"]);
}

#[tokio::test]
async fn versioned_policy_roundtrips_through_authorized_cas_api() {
    let fixture = codex_route_fixture("policy-api-contract").await;
    set_policy(
        &fixture,
        json!({"version": 1, "candidate_attempts": 8,
        "failover_deadline_millis": 1234}),
    )
    .await;
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
