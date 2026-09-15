use super::*;

#[tokio::test]
async fn native_cursor_never_uses_openai_http_fallback() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices":[{"message":{"role":"assistant","content":"not Cursor protocol"}}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
        })))
        .expect(0)
        .mount(&upstream)
        .await;
    let fixture =
        resilient_route_fixture("native-cursor-unsupported", &[(upstream.uri(), 0)]).await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    // Keep an existing granted route to prove the final dispatch boundary,
    // independently of future route-configuration capability checks.
    sqlx::query("UPDATE upstream_accounts SET driver = 'cursor' WHERE id = $1")
        .bind(fixture.accounts[0].to_string())
        .execute(&pool)
        .await
        .unwrap();
    for stream in [false, true] {
        let response = send_resilient_chat(&fixture, None, stream).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let _ = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
    upstream.verify().await;
    pool.close().await;
}
