use super::*;

#[tokio::test]
async fn early_rejections_return_server_correlation_without_creating_request_records() {
    let fixture = codex_route_fixture("phase-early-rejections").await;
    let supplied_id = Uuid::new_v4();
    for (path, authorized, body, expected) in [
        ("/v1/responses", false, "{}", StatusCode::UNAUTHORIZED),
        ("/v1/responses", true, "not-json", StatusCode::BAD_REQUEST),
        ("/v1/responses/compact", true, "{}", StatusCode::NOT_FOUND),
    ] {
        let mut request = Request::post(path)
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-request-id", supplied_id.to_string())
            .header(REQUEST_ID_HEADER, supplied_id.to_string());
        if authorized {
            request = request.header(header::AUTHORIZATION, format!("Bearer {}", fixture.key));
        }
        let response = router_for_role(fixture.state.clone(), RuntimeRole::Gateway)
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        let id = Uuid::parse_str(
            response
                .headers()
                .get(REQUEST_ID_HEADER)
                .unwrap()
                .to_str()
                .unwrap(),
        )
        .unwrap();
        assert_ne!(
            id, supplied_id,
            "caller IDs must not own diagnostic/admission identity"
        );
    }
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_records")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count, 0,
        "pre-admission diagnostics must not add database writes"
    );
    pool.close().await;
}

#[tokio::test]
async fn successful_proxy_response_and_durable_record_keep_the_same_correlation() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "synthetic-response", "object": "chat.completion", "model": "synthetic",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        }))).expect(1).mount(&upstream).await;
    let fixture = resilient_route_fixture("phase-correlation", &[(upstream.uri(), 0)]).await;
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let id = response
        .headers()
        .get(REQUEST_ID_HEADER)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let _ = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_records WHERE id = $1 AND completed_at IS NOT NULL",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    upstream.verify().await;
    pool.close().await;
}
