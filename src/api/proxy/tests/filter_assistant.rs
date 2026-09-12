use super::*;

async fn management_request(
    fixture: &CodexRouteFixture,
    path: &str,
    method: &str,
    body: Value,
) -> Response {
    router_for_role(fixture.state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", fixture.state.config.service_token),
                )
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
    let fixture = response_usage_fixture("filter-assistant", &upstream, 0).await;
    let tenant = "compatibility-route-filter-assistant";
    let settings = json!({"tenant_external_id":tenant,"model_route_id":fixture.route_id,"billing_key_id":fixture.key_id,"expected_updated_at":null});
    let saved = management_request(
        &fixture,
        "/internal/v1/filter-assistant/settings",
        "PUT",
        settings.clone(),
    )
    .await;
    assert_eq!(saved.status(), StatusCode::OK);
    let conflict = management_request(
        &fixture,
        "/internal/v1/filter-assistant/settings",
        "PUT",
        settings,
    )
    .await;
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    let body =
        json!({"tenant_external_id":tenant,"prompt":"Find requests slower than 2750 milliseconds"});
    let response = management_request(
        &fixture,
        "/internal/v1/filter-assistant/plan",
        "POST",
        body.clone(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let plan: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(plan["model_route_id"], fixture.route_id.to_string());
    assert_eq!(plan["ast"], suggestion);
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let recorded: (String, i64) =
        sqlx::query_as("SELECT model_route_id, cost_micros FROM request_records WHERE key_id = $1")
            .bind(fixture.key_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(recorded.0, fixture.route_id.to_string());
    assert!(recorded.1 > 0);
    sqlx::query("UPDATE model_routes SET enabled = 0 WHERE id = $1")
        .bind(fixture.route_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let disabled =
        management_request(&fixture, "/internal/v1/filter-assistant/plan", "POST", body).await;
    assert_eq!(disabled.status(), StatusCode::BAD_REQUEST);
    upstream.verify().await;
    pool.close().await;
}
