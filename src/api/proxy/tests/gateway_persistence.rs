use super::*;

#[tokio::test]
async fn unavailable_request_archive_does_not_change_sse_or_exactly_once_settlement() {
    request_archive_degradation_preserves_sse(false).await;
}

#[tokio::test]
async fn saturated_request_archive_does_not_change_sse_or_exactly_once_settlement() {
    request_archive_degradation_preserves_sse(true).await;
}

async fn request_archive_degradation_preserves_sse(saturated: bool) {
    let fixture = codex_route_fixture("optional-request-archive-failure").await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_optional_capture BEFORE INSERT ON request_archive_spool_chunks BEGIN SELECT RAISE(ABORT, 'synthetic archive failure'); END")
        .execute(&pool).await.unwrap();
    let capacity = if saturated {
        Some(
            fixture
                .state
                .db
                .saturate_gateway_persistence_for_test()
                .await,
        )
    } else {
        None
    };
    let upstream = MockServer::start().await;
    let sse = completed_codex_sse("archive failure must not change delivery");
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse.clone(), "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({
            "model": fixture.model, "input": "optional persistence unavailable", "stream": true,
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap()
            .as_ref(),
        sse.as_bytes()
    );
    wait_for_request_settlement(&fixture, 1).await;
    drop(capacity);
    fixture.state.db.drain_gateway_persistence_for_test().await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(200));
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-codex")).await;
    let locator: String =
        sqlx::query_scalar("SELECT request_object FROM request_records WHERE id = $1")
            .bind(rows[0].request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(locator, format!("gap://{}/request", rows[0].request_id));
    upstream.verify().await;
    pool.close().await;
}
