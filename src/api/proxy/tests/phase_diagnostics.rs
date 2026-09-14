use super::*;

#[tokio::test]
async fn early_rejections_return_server_correlation_without_creating_request_records() {
    let fixture = codex_route_fixture("phase-early-rejections").await;
    let supplied_id = Uuid::new_v4();
    for (path, authorized, body, expected) in [
        ("/v1/responses", false, "{}", StatusCode::UNAUTHORIZED),
        ("/v1/responses", true, "not-json", StatusCode::BAD_REQUEST),
        ("/v1/responses/compact", true, "{}", StatusCode::NOT_FOUND),
        (
            "/v1/responses",
            true,
            r#"{"model":"unconfigured-diagnostic-model","input":"synthetic"}"#,
            StatusCode::FORBIDDEN,
        ),
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
    // Keep the enabled, granted model route: authorization must pass. Only
    // remove the account from the candidate query's active-account set.
    sqlx::query("UPDATE upstream_accounts SET status = 'disabled' WHERE id = $1")
        .bind(fixture.upstream_account_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let response = router_for_role(fixture.state.clone(), RuntimeRole::Gateway)
        .oneshot(
            Request::post("/v1/responses")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {}", fixture.key))
                .header(REQUEST_ID_HEADER, supplied_id.to_string())
                .body(Body::from(
                    json!({"model": fixture.model, "input": "synthetic"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let id = Uuid::parse_str(response.headers()[REQUEST_ID_HEADER].to_str().unwrap()).unwrap();
    assert_ne!(id, supplied_id);
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
async fn control_early_failures_have_distinct_server_ids_without_proxy_records() {
    let fixture = codex_route_fixture("phase-control-rejections").await;
    let supplied = Uuid::new_v4();
    let mut ids = std::collections::HashSet::new();
    for (method, path) in [
        ("POST", "/internal/v1/requests/query".to_owned()),
        ("GET", format!("/internal/v1/requests/{}", Uuid::new_v4())),
        ("GET", "/internal/v1/upstreams".to_owned()),
        ("GET", "/internal/v1/request-events".to_owned()),
    ] {
        let response = router_for_role(fixture.state.clone(), RuntimeRole::Control)
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(REQUEST_ID_HEADER, supplied.to_string())
                    .header("x-request-id", supplied.to_string())
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.status().is_client_error());
        let id = Uuid::parse_str(response.headers()[REQUEST_ID_HEADER].to_str().unwrap()).unwrap();
        assert_ne!(id, supplied);
        assert!(ids.insert(id));
    }
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_records")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
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
