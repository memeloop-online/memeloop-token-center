use super::*;

async fn management_request(
    fixture: &CodexRouteFixture,
    path: &str,
    method: &str,
    body: Value,
) -> Response {
    management_request_with_token(
        fixture,
        path,
        method,
        body,
        &fixture.state.config.service_token,
    )
    .await
}

async fn management_request_with_token(
    fixture: &CodexRouteFixture,
    path: &str,
    method: &str,
    body: Value,
    token: &str,
) -> Response {
    router_for_role(fixture.state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn assistant_invokes_selected_route_and_bills_explicit_key_then_rejects_disabled_route() {
    let upstream = MockServer::start().await;
    let suggestion = json!({"logical_operator":"and","conditions":[{"field":"duration_ms","operator":"greater_than","value":{"type":"integer","value":2750}}]});
    Mock::given(method("POST")).and(path("/v1/responses"))
        .and(body_partial_json(json!({"model":"gpt-5.6-sol","stream":false,"max_output_tokens":2048})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"resp_filter", "object":"response", "status":"completed", "model":"gpt-5.6-sol",
            "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":suggestion.to_string()}]}],
            "usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130}
        }))).expect(1).mount(&upstream).await;
    let mut fixture = response_usage_fixture("filter-assistant", &upstream, 0).await;
    let tenant = "compatibility-route-filter-assistant";
    // Both routes are authorized for the same public model. Ordinary priority
    // selection would choose the decoy; the configured route must win instead.
    let decoy = fixture
        .state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant.into(),
            public_model: fixture.model.clone(),
            upstream_account_id: fixture.upstream_account_id,
            upstream_model: "must-not-be-selected".into(),
            protocol: "openai".into(),
            priority: -100,
        })
        .await
        .unwrap();
    let billed = fixture
        .state
        .db
        .create_key_with_routing(
            CreateKeyInput {
                tenant_external_id: tenant.into(),
                principal_external_id: "assistant-owner".into(),
                alias: "assistant-billing".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ONE,
                idempotency_key: None,
            },
            &[fixture.route_id, decoy.id],
            &[],
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    fixture.key_id = billed.key_id;
    fixture.key = billed.key;
    let settings = json!({"tenant_external_id":tenant,"model_route_id":fixture.route_id,"billing_key_id":fixture.key_id,"expected_updated_at":null});
    let saved = management_request(
        &fixture,
        "/internal/v1/filter-assistant/settings",
        "PUT",
        settings.clone(),
    )
    .await;
    assert_eq!(saved.status(), StatusCode::OK);
    let saved: Value =
        serde_json::from_slice(&to_bytes(saved.into_body(), 8192).await.unwrap()).unwrap();
    let settings_updated_at = saved["updated_at"].as_i64().unwrap();
    let conflict = management_request(
        &fixture,
        "/internal/v1/filter-assistant/settings",
        "PUT",
        settings,
    )
    .await;
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    let reader = fixture
        .state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "filter-assistant-reader".into(),
                scopes: vec!["requests:read".into()],
                tenant_external_id: Some(tenant.into()),
            },
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let executor = fixture
        .state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "filter-assistant-executor".into(),
                scopes: vec!["filter_assistant:execute".into()],
                tenant_external_id: Some(tenant.into()),
            },
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let body =
        json!({"tenant_external_id":tenant,"prompt":"Find requests slower than 2750 milliseconds"});
    let denied = management_request_with_token(
        &fixture,
        "/internal/v1/filter-assistant/plan",
        "POST",
        body.clone(),
        &reader.token,
    )
    .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let wrong_tenant = management_request_with_token(
        &fixture,
        "/internal/v1/filter-assistant/plan",
        "POST",
        json!({"tenant_external_id":"another-tenant","prompt":"Find slow requests"}),
        &executor.token,
    )
    .await;
    assert_eq!(wrong_tenant.status(), StatusCode::FORBIDDEN);
    let response = management_request_with_token(
        &fixture,
        "/internal/v1/filter-assistant/plan",
        "POST",
        body.clone(),
        &executor.token,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let plan: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(plan["model_route_id"], fixture.route_id.to_string());
    assert_eq!(plan["ast"], suggestion);
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let recorded = sqlx::query("SELECT q.model_route_id, q.cost_micros, q.status_code, q.reservation_id, r.status AS reservation_status, r.actual_micros FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id WHERE q.key_id = $1")
        .bind(fixture.key_id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        recorded.get::<String, _>("model_route_id"),
        fixture.route_id.to_string()
    );
    let cost_micros = recorded.get::<i64, _>("cost_micros");
    assert!(cost_micros > 0);
    assert_eq!(recorded.get::<i64, _>("status_code"), 200);
    assert_eq!(recorded.get::<String, _>("reservation_status"), "settled");
    assert_eq!(recorded.get::<i64, _>("actual_micros"), cost_micros);
    let reservation_id = recorded.get::<String, _>("reservation_id");
    let usage_ledger_entries: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ledger_entries WHERE key_id = $1 AND kind = 'usage' AND source = $2",
    )
    .bind(fixture.key_id.to_string())
    .bind(reservation_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(usage_ledger_entries, 1);
    let exhausted = fixture
        .state
        .db
        .create_key_with_routing(
            CreateKeyInput {
                tenant_external_id: tenant.into(),
                principal_external_id: "assistant-exhausted-owner".into(),
                alias: "assistant-exhausted".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ZERO,
                idempotency_key: None,
            },
            &[fixture.route_id],
            &[],
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let replaced = management_request(
        &fixture,
        "/internal/v1/filter-assistant/settings",
        "PUT",
        json!({"tenant_external_id":tenant,"model_route_id":fixture.route_id,"billing_key_id":exhausted.key_id,"expected_updated_at":settings_updated_at}),
    )
    .await;
    assert_eq!(replaced.status(), StatusCode::OK);
    let budget_denied = management_request_with_token(
        &fixture,
        "/internal/v1/filter-assistant/plan",
        "POST",
        body.clone(),
        &executor.token,
    )
    .await;
    assert_eq!(budget_denied.status(), StatusCode::TOO_MANY_REQUESTS);
    let exhausted_requests: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_records WHERE key_id = $1")
            .bind(exhausted.key_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(exhausted_requests, 0);
    sqlx::query("UPDATE model_routes SET enabled = 0 WHERE id = $1")
        .bind(fixture.route_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let disabled = management_request_with_token(
        &fixture,
        "/internal/v1/filter-assistant/plan",
        "POST",
        body,
        &executor.token,
    )
    .await;
    assert_eq!(disabled.status(), StatusCode::BAD_REQUEST);
    upstream.verify().await;
    pool.close().await;
}
