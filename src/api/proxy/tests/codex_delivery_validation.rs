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
