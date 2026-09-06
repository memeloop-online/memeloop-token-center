use super::*;

#[tokio::test]
async fn codex_2xx_non_sse_is_ambiguous_and_never_crosses_accounts() {
    let fixture = codex_route_fixture("ambiguous-non-sse").await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-123"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"error":{"message":"temporary high demand secret"}}"#,
            "application/json",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-456"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(completed_codex_sse("must not run"), "text/event-stream"),
        )
        .expect(0)
        .mount(&upstream)
        .await;

    add_codex_standby_route(&fixture, "codex-route-ambiguous-non-sse", "account-456").await;
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "ambiguous", "stream": false}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("upstream request failed"));
    assert!(!String::from_utf8_lossy(&body).contains("high demand secret"));
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let actual: String = sqlx::query_scalar(
        "SELECT upstream_account_id FROM request_records WHERE key_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(fixture.key_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(actual, fixture.upstream_account_id.to_string());
    let failure_kind: String = sqlx::query_scalar(
        "SELECT last_failure_kind FROM upstream_account_health WHERE upstream_account_id = $1",
    )
    .bind(fixture.upstream_account_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(failure_kind, "connection");
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status_code, Some(502));
    assert_eq!(
        rows[0].error_code.as_deref(),
        Some("upstream_invalid_content_type")
    );
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
    for row in rows {
        let refs = fixture
            .state
            .db
            .request_archive_refs(fixture.key_id, row.request_id)
            .await
            .unwrap();
        let archived = fixture
            .state
            .archive
            .get(refs.response_object.as_deref().unwrap())
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&archived).contains("temporary high demand secret"));
    }
    pool.close().await;
    upstream.verify().await;
}

#[tokio::test]
async fn buffered_codex_output_with_malformed_usage_is_rejected_before_delivery() {
    let fixture = codex_route_fixture("buffered-malformed-usage").await;
    let upstream = MockServer::start().await;
    let body = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-malformed-usage\"}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,",
        "\"item\":{\"id\":\"item-private\",\"type\":\"message\",\"role\":\"assistant\",",
        "\"content\":[{\"type\":\"output_text\",\"text\":\"private billable output\"}]}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-malformed-usage\",",
        "\"output\":[],\"usage\":{\"input_tokens\":3,\"total_tokens\":3}}}\n\n"
    );
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;

    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "billable", "stream": false}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let response_body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&response_body).contains("private billable output"));
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].error_code.as_deref(),
        Some("upstream_invalid_usage")
    );
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
    assert_response_archives_omit(&fixture, "private billable output").await;
    upstream.verify().await;
}

#[tokio::test]
async fn buffered_codex_cache_usage_is_settled_at_distinct_prices() {
    let fixture = codex_route_fixture("buffered-cache-pricing").await;
    fixture
        .state
        .db
        .upsert_model_price_tier(
            &fixture.model,
            "USD",
            "default",
            Decimal::ONE,
            Decimal::from(2),
            Decimal::from(3),
            Decimal::from(4),
            false,
        )
        .await
        .unwrap();
    let upstream = MockServer::start().await;
    let body = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-cache-priced\"}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,",
        "\"item\":{\"id\":\"item-cache-priced\",\"type\":\"message\",\"role\":\"assistant\",",
        "\"content\":[{\"type\":\"output_text\",\"text\":\"priced\"}]}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{",
        "\"id\":\"resp-cache-priced\",\"output\":[],",
        "\"usage\":{\"input_tokens\":10,\"input_tokens_details\":{\"cached_tokens\":3,",
        "\"cache_write_tokens\":2},\"output_tokens\":1,\"output_tokens_details\":null,",
        "\"total_tokens\":11}}}\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;

    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "price cache categories", "stream": false}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response_body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&response_body).unwrap()["id"],
        "resp-cache-priced"
    );

    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    // Request records retain the upstream's inclusive input total. Cached and
    // cache-write tokens are persisted separately and the exact-cost checks
    // below prove that the three categories were settled at distinct prices.
    assert_eq!(rows[0].input_tokens, 10);
    assert_eq!(rows[0].cached_input_tokens, 3);
    assert_eq!(rows[0].cache_write_tokens, 2);
    assert_eq!(rows[0].output_tokens, 1);
    assert_eq!(rows[0].cost, "0.000021");

    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let stored = sqlx::query(
        "SELECT r.actual_micros, c.available_micros
         FROM request_records q
         JOIN usage_reservations r ON r.id = q.reservation_id
         JOIN key_records k ON k.id = q.key_id
         JOIN credit_accounts c ON c.id = k.account_id
         WHERE q.id = $1",
    )
    .bind(rows[0].request_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored.get::<i64, _>("actual_micros"), 21);
    assert_eq!(stored.get::<i64, _>("available_micros"), 999_979);
    pool.close().await;
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-cache-priced")).await;
    upstream.verify().await;
}

#[tokio::test]
async fn streaming_codex_cache_usage_is_settled_at_distinct_prices() {
    let fixture = codex_route_fixture("streaming-cache-pricing").await;
    fixture
        .state
        .db
        .upsert_model_price_tier(
            &fixture.model,
            "USD",
            "default",
            Decimal::ONE,
            Decimal::from(2),
            Decimal::from(3),
            Decimal::from(4),
            false,
        )
        .await
        .unwrap();
    let upstream = MockServer::start().await;
    let body = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-stream-cache\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{",
        "\"id\":\"resp-stream-cache\",\"output\":[],",
        "\"usage\":{\"input_tokens\":10,\"input_tokens_details\":{\"cached_tokens\":3,",
        "\"cache_write_tokens\":2},\"output_tokens\":1,\"output_tokens_details\":null,",
        "\"total_tokens\":11}}}\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;

    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "stream price categories", "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("resp-stream-cache"));
    wait_for_request_settlement(&fixture, 1).await;

    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status_code, Some(200));
    // See the buffered counterpart: this is the upstream's inclusive input
    // total, while the cache categories below remain independently priced.
    assert_eq!(rows[0].input_tokens, 10);
    assert_eq!(rows[0].cached_input_tokens, 3);
    assert_eq!(rows[0].cache_write_tokens, 2);
    assert_eq!(rows[0].output_tokens, 1);
    assert_eq!(rows[0].cost, "0.000021");
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let stored = sqlx::query(
        "SELECT r.actual_micros, c.available_micros
         FROM request_records q
         JOIN usage_reservations r ON r.id = q.reservation_id
         JOIN key_records k ON k.id = q.key_id
         JOIN credit_accounts c ON c.id = k.account_id
         WHERE q.id = $1",
    )
    .bind(rows[0].request_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored.get::<i64, _>("actual_micros"), 21);
    assert_eq!(stored.get::<i64, _>("available_micros"), 999_979);
    pool.close().await;
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-stream-cache")).await;
    upstream.verify().await;
}

#[tokio::test]
async fn streaming_codex_accepts_nullable_usage_details() {
    let fixture = codex_route_fixture("streaming-null-usage-details").await;
    let upstream = MockServer::start().await;
    let body = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-null-details\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{",
        "\"id\":\"resp-null-details\",\"output\":[],",
        "\"usage\":{\"input_tokens\":4,\"input_tokens_details\":null,",
        "\"output_tokens\":1,\"output_tokens_details\":null,\"total_tokens\":5}}}\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;

    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "nullable details", "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status_code, Some(200));
    assert_eq!(rows[0].input_tokens, 4);
    assert_eq!(rows[0].cached_input_tokens, 0);
    assert_eq!(rows[0].cache_write_tokens, 0);
    assert_eq!(rows[0].output_tokens, 1);
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-null-details")).await;
    upstream.verify().await;
}

#[tokio::test]
async fn streaming_codex_rejects_usage_without_canonical_total_after_delivery() {
    let fixture = codex_route_fixture("streaming-missing-usage-total").await;
    let upstream = MockServer::start().await;
    let body = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-missing-total\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{",
        "\"id\":\"resp-missing-total\",\"output\":[],",
        "\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;

    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "missing total", "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status_code, Some(502));
    assert_eq!(
        rows[0].error_code.as_deref(),
        Some("upstream_invalid_usage")
    );
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
    upstream.verify().await;
}

#[tokio::test]
async fn streaming_codex_rejects_output_items_before_response_identity() {
    let fixture = codex_route_fixture("streaming-item-before-id").await;
    let upstream = MockServer::start().await;
    let body = concat!(
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,",
        "\"item\":{\"id\":\"item-private\",\"type\":\"message\",\"role\":\"assistant\",",
        "\"content\":[{\"type\":\"output_text\",\"text\":\"must-not-deliver\"}]}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-too-late\",",
        "\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n"
    );
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;

    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "identity first", "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .is_err(),
        "the invalid output item must not become a downstream body"
    );

    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status_code, Some(502));
    assert_eq!(
        rows[0].error_code.as_deref(),
        Some("upstream_invalid_response")
    );
    assert_eq!(rows[0].cost, "0");
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
    let refs = fixture
        .state
        .db
        .request_archive_refs(fixture.key_id, rows[0].request_id)
        .await
        .unwrap();
    assert!(
        refs.response_object
            .as_deref()
            .is_some_and(|locator| locator.starts_with("gap://"))
    );
    upstream.verify().await;
}
