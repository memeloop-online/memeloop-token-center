use super::*;

#[tokio::test]
async fn actual_upstream_price_and_model_survive_route_and_price_edits() {
    let fixture = codex_route_fixture("immutable-actual-price").await;
    fixture
        .state
        .db
        .upsert_model_price(&fixture.model, "USD", Decimal::from(99), Decimal::from(99))
        .await
        .unwrap();
    let expected_price = fixture
        .state
        .db
        .model_price(&fixture.upstream_model, "USD")
        .await
        .unwrap();
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(completed_codex_sse("snapshot"), "text/event-stream"),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "snapshot", "stream": false}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    wait_for_request_settlement(&fixture, 1).await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let stored = sqlx::query("SELECT request.upstream_model, reservation.price_snapshot_json FROM request_records request JOIN usage_reservations reservation ON reservation.id = request.reservation_id WHERE request.key_id = $1")
        .bind(fixture.key_id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(
        stored.get::<String, _>("upstream_model"),
        fixture.upstream_model
    );
    let snapshot: crate::model::ModelPrice =
        serde_json::from_str(&stored.get::<String, _>("price_snapshot_json")).unwrap();
    assert_eq!(snapshot.id, expected_price.id);
    sqlx::query("UPDATE model_routes SET upstream_model = 'edited-after-settlement' WHERE id = $1")
        .bind(fixture.route_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE model_route_upstream_accounts SET upstream_model = 'edited-after-settlement' WHERE model_route_id = $1")
        .bind(fixture.route_id.to_string()).execute(&pool).await.unwrap();
    fixture
        .state
        .db
        .upsert_model_price(
            &fixture.upstream_model,
            "USD",
            Decimal::from(77),
            Decimal::from(77),
        )
        .await
        .unwrap();
    let requests = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].upstream_model.as_deref(),
        Some(fixture.upstream_model.as_str())
    );
    assert_eq!(requests[0].model, fixture.model);
    assert_eq!(requests[0].cost, "0.000005");
    upstream.verify().await;
}
