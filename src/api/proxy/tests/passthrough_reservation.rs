use super::*;

async fn fixture(label: &str, upstream: &MockServer, driver: &str) -> CodexRouteFixture {
    let fixture = response_usage_fixture_with_uri_contract_and_driver(
        label,
        upstream.uri(),
        256,
        None,
        driver,
    )
    .await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let config = json!({
        "base_url": upstream.uri(),
        "network_scope": "public",
        "input_token_overhead_ceiling": 256,
        "reservation_token_bounds": {fixture.upstream_model.clone(): 65_536}
    });
    sqlx::query("UPDATE upstream_accounts SET config_json = $1 WHERE id = $2")
        .bind(config.to_string())
        .bind(fixture.upstream_account_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    fixture
}

async fn send(fixture: &CodexRouteFixture, limit: Option<i64>) -> Response {
    let mut request = json!({
        "model": fixture.model,
        "input": "hello",
        "tools": [{"type": "function", "name": "spawn_agent",
            "parameters": {"type": "object", "properties": {}}}],
        "stream": true
    });
    if let Some(limit) = limit {
        request["max_output_tokens"] = json!(limit);
    }
    router_for_role(fixture.state.clone(), RuntimeRole::Gateway)
        .oneshot(
            Request::post("/v1/responses")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::USER_AGENT, "codex_cli_rs/0.155.0")
                .header(header::AUTHORIZATION, format!("Bearer {}", fixture.key))
                .body(Body::from(serde_json::to_vec(&request).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn passthrough_reserves_trusted_bound_and_settles_actual_usage_above_4096() {
    for driver in ["http-json", "new-api"] {
        let upstream = MockServer::start().await;
        let completed = completed_response_with_usage(3, 5_000);
        let sse = format!(
            "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp-usage-contract\"}}}}\n\n\
             data: {{\"type\":\"response.completed\",\"response\":{completed}}}\n\n\
             data: [DONE]\n\n"
        );
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .and(|request: &wiremock::Request| {
                let value: Value = serde_json::from_slice(&request.body).unwrap();
                value.get("max_output_tokens").is_none()
            })
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
            .expect(1)
            .mount(&upstream)
            .await;
        let fixture = fixture(driver, &upstream, driver).await;
        let response = send(&fixture, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let _body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        wait_for_request_settlement(&fixture, 1).await;
        let rows = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(rows[0].status_code, Some(200));
        assert_eq!(rows[0].error_code, None);
        assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (3, 5_000));
        assert_eq!(
            rows[0].cost.parse::<Decimal>().unwrap(),
            Decimal::new(5003, 6)
        );
        assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-usage-contract"))
            .await;
        let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
        let reserved: i64 = sqlx::query_scalar("SELECT reserved_tokens FROM usage_reservations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(reserved >= 65_536);
        pool.close().await;
        upstream.verify().await;
    }
}

#[tokio::test]
async fn passthrough_explicit_limit_remains_a_settlement_constraint() {
    let upstream = MockServer::start().await;
    let completed = completed_response_with_usage(3, 5_000);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(body_partial_json(json!({"max_output_tokens": 4096})))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!(
                "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp-usage-contract\"}}}}\n\n\
                 data: {{\"type\":\"response.completed\",\"response\":{completed}}}\n\n\
                 data: [DONE]\n\n"
            ),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = fixture("explicit-limit", &upstream, "http-json").await;
    let response = send(&fixture, Some(4096)).await;
    let _body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(502));
    assert_eq!(
        rows[0].error_code.as_deref(),
        Some("upstream_invalid_usage")
    );
    assert_eq!(rows[0].output_tokens, 0);
    upstream.verify().await;
}

#[test]
fn passthrough_bounds_fail_closed_on_invalid_present_metadata() {
    let bound = super::super::routing::passthrough_output_reservation_bound;
    assert_eq!(bound(&json!({}), &json!({}), "model").unwrap(), 4096);
    for config in [
        json!({"reservation_token_bounds": {"model": -1}}),
        json!({"reservation_token_bounds": {"model": 1_000_000_001_i64}}),
        json!({"reservation_token_bounds": {"model": null}}),
        json!({"reservation_token_bounds": {"model": "65536"}}),
        json!({"reservation_token_bounds": null}),
        json!({"reservation_token_bounds": []}),
    ] {
        assert!(bound(&json!({}), &config, "model").is_err());
    }
    for limit in [json!(-1), json!(1.5), json!("65536"), Value::Null] {
        assert!(bound(&json!({"max_output_tokens": limit}), &json!({}), "model").is_err());
    }
}

#[test]
fn optional_passthrough_bounds_only_change_listed_models_and_keep_native_codex_strict() {
    let bound = super::super::routing::passthrough_output_reservation_bound;
    let config = json!({"reservation_token_bounds": {"listed-model": 65536}});
    assert_eq!(bound(&json!({}), &config, "listed-model").unwrap(), 65536);
    assert_eq!(bound(&json!({}), &config, "unlisted-model").unwrap(), 4096);
    assert_eq!(
        bound(
            &json!({}),
            &json!({"reservation_token_bounds": {}}),
            "unlisted-model"
        )
        .unwrap(),
        4096
    );
    for model in ["listed-model", "unlisted-model"] {
        assert_eq!(
            bound(&json!({"max_output_tokens": 123}), &config, model).unwrap(),
            123
        );
    }
    // This fallback is restricted to generic pass-through accounts. Native
    // Codex must still reject a route without synchronized model metadata.
    assert!(codex_transport::trusted_reservation_token_bound(&config, "unlisted-model").is_err());
    assert_eq!(
        codex_transport::trusted_reservation_token_bound(&config, "listed-model").unwrap(),
        65536
    );
}

#[tokio::test]
async fn passthrough_bound_must_be_funded_before_dispatch() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;
    let fixture = fixture("unfunded-bound", &upstream, "http-json").await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    // Enough for the historical 4096-token bound, not the trusted 65536.
    sqlx::query("UPDATE credit_accounts SET available_micros = 10000 WHERE id = $1")
        .bind(fixture.credit_account_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let response = send(&fixture, None).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("balance_exhausted"));
    let reservations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_reservations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(reservations, 0);
    pool.close().await;
    upstream.verify().await;
}
