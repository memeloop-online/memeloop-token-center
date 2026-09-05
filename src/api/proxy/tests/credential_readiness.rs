use super::*;

async fn expire_current_credential_metadata(fixture: &CodexRouteFixture) {
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query(
        "UPDATE upstream_credentials SET expires_at = $1
         WHERE upstream_account_id = $2
           AND generation = (SELECT credential_generation FROM upstream_accounts WHERE id = $2)",
    )
    .bind(crate::db::unix_millis())
    .bind(fixture.upstream_account_id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn expired_primary_credential_is_filtered_before_healthy_standby() {
    let fixture = codex_route_fixture("expired-primary").await;
    let standby =
        add_codex_standby_route(&fixture, "codex-route-expired-primary", "account-456").await;
    expire_current_credential_metadata(&fixture).await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-123"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-456"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(completed_codex_sse("healthy standby"), "text/event-stream"),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "expired primary", "stream": false}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let actual: String = sqlx::query_scalar(
        "SELECT upstream_account_id FROM request_records WHERE key_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(fixture.key_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(actual, standby.to_string());
    pool.close().await;
    upstream.verify().await;
}

#[tokio::test]
async fn credential_expiring_after_resolution_skips_to_prepared_standby() {
    let fixture = codex_route_fixture("expiry-prepare-race").await;
    let standby =
        add_codex_standby_route(&fixture, "codex-route-expiry-prepare-race", "account-456").await;
    let key = fixture
        .state
        .db
        .authenticate_key(&fixture.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let request = json!({"model": fixture.model, "input": "expiry race", "stream": false});
    let mut candidates = fixture
        .state
        .db
        .resolve_authorized_upstream_candidates_with_hint(
            key.key_id,
            key.tenant_id,
            &fixture.model,
            Protocol::OpenAiResponses.name(),
            RouteSelectionOptions {
                upstream_account_hint: None,
                selection_seed: request_id,
            },
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    assert_eq!(candidates[0].account_id, fixture.upstream_account_id);
    let UpstreamCredential::OAuth { expires_at, .. } = &mut candidates[0].credential else {
        panic!("Codex fixture uses OAuth");
    };
    *expires_at = Some(crate::db::unix_millis());

    let prepared = prepare_authorized_proxy_routes(
        &fixture.state,
        &key,
        &fixture.model,
        Protocol::OpenAiResponses,
        request_id,
        &request,
        candidates,
    )
    .await
    .unwrap();
    assert_eq!(prepared.len(), 1);
    assert_eq!(prepared[0].route.account_id, standby);
}
