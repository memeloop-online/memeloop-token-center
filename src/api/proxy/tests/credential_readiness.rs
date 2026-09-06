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
        serde_json::to_vec(&request).unwrap().len(),
        candidates,
    )
    .await
    .unwrap();
    assert_eq!(prepared.direct_candidates.len(), 1);
    assert_eq!(prepared.direct_candidates[0].account_id, standby);
}

#[tokio::test]
async fn local_codex_protocol_mismatch_skips_to_compatible_candidate() {
    let fixture = codex_route_fixture("local-protocol-mismatch").await;
    let key = fixture
        .state
        .db
        .authenticate_key(&fixture.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let mut candidates = fixture
        .state
        .db
        .resolve_authorized_upstream_candidates_with_hint(
            key.key_id,
            key.tenant_id,
            &fixture.model,
            Protocol::OpenAiChat.name(),
            RouteSelectionOptions {
                upstream_account_hint: None,
                selection_seed: request_id,
            },
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    assert_eq!(candidates.len(), 1);
    let mut compatible = candidates[0].clone();
    compatible.route_id = Uuid::now_v7();
    compatible.account_id = Uuid::now_v7();
    compatible.driver = "http-json".to_owned();
    compatible.base_url = "https://example.com".to_owned();
    compatible.config = json!({"base_url": compatible.base_url.clone(), "network_scope": "public"});
    compatible.credential = UpstreamCredential::None;
    candidates.push(compatible.clone());

    let request = json!({
        "model": fixture.model,
        "messages": [{"role": "user", "content": "local mismatch"}],
        "stream": false
    });
    let prepared = prepare_authorized_proxy_routes(
        &fixture.state,
        &key,
        &fixture.model,
        Protocol::OpenAiChat,
        request_id,
        &request,
        serde_json::to_vec(&request).unwrap().len(),
        candidates,
    )
    .await
    .unwrap();
    assert_eq!(prepared.direct_candidates.len(), 1);
    assert_eq!(
        prepared.direct_candidates[0].account_id,
        compatible.account_id
    );
}

#[tokio::test]
async fn changed_transport_revision_invalidates_the_prepared_snapshot() {
    let fixture = codex_route_fixture("transport-snapshot-fence").await;
    let key = fixture
        .state
        .db
        .authenticate_key(&fixture.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let mut candidate = fixture
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
        .unwrap()
        .remove(0);
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE upstream_accounts SET updated_at = updated_at + 1 WHERE id = $1")
        .bind(fixture.upstream_account_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    assert_eq!(
        refresh_route_snapshot(&fixture.state, &mut candidate)
            .await
            .unwrap(),
        PreparedRouteReadiness::Unavailable
    );
}

#[test]
fn credential_expiring_at_header_application_is_typed_unavailable() {
    let now = crate::db::unix_millis();
    let credential = UpstreamCredential::OAuth {
        access_token: "expired".to_owned(),
        refresh_token: None,
        expires_at: Some(now),
        header: "authorization".to_owned(),
        prefix: "Bearer ".to_owned(),
        adapter_state: None,
        proxy_url: None,
        proxy_network_scope: None,
    };
    assert!(matches!(
        credential_application_error(&credential, now),
        ProxySendError::CredentialUnavailable
    ));
}

#[tokio::test]
async fn cooldown_candidates_do_not_consume_the_three_outbound_attempts() {
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    let third = MockServer::start().await;
    let fourth = MockServer::start().await;
    for upstream in [&first, &second, &third] {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(upstream)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(successful_chat_response())
        .expect(1)
        .mount(&fourth)
        .await;
    let fixture = resilient_route_fixture(
        "three-cooldowns-fourth-healthy",
        &[
            (first.uri(), 0),
            (second.uri(), 10),
            (third.uri(), 20),
            (fourth.uri(), 30),
        ],
    )
    .await;
    for account_id in fixture.accounts.iter().take(3) {
        assert!(
            fixture
                .state
                .db
                .record_upstream_account_failure(*account_id, 1, UpstreamFailureKind::Connection)
                .await
                .unwrap()
        );
    }

    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    for upstream in [&first, &second, &third, &fourth] {
        upstream.verify().await;
    }
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let actual: String = sqlx::query_scalar(
        "SELECT upstream_account_id FROM request_records WHERE key_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(fixture.key_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(actual, fixture.accounts[3].to_string());
    pool.close().await;
}

#[tokio::test]
async fn header_application_expiry_skips_to_standby_without_breaker_failure() {
    let fixture = codex_route_fixture("header-expiry-standby").await;
    let standby =
        add_codex_standby_route(&fixture, "codex-route-header-expiry-standby", "account-456").await;
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
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("standby after header expiry"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;

    let response = routing::with_test_credential_application_now_once(
        i64::MAX,
        send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model": fixture.model, "input": "header expiry", "stream": false}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    upstream.verify().await;

    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let actual: String = sqlx::query_scalar(
        "SELECT upstream_account_id FROM request_records WHERE key_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(fixture.key_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    let primary_failure_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM upstream_account_health WHERE upstream_account_id = $1",
    )
    .bind(fixture.upstream_account_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(actual, standby.to_string());
    assert_eq!(primary_failure_count, 0);
    pool.close().await;
}
