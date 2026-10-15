use super::group_custom::{group, members};
use super::*;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

async fn request(state: &AppState, key: &str, endpoint: &str, body: Value) -> (StatusCode, Value) {
    let response = api::router(state.clone())
        .oneshot(
            Request::post(endpoint)
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

async fn financial_counts(pool: &sqlx::AnyPool, key_id: Uuid) -> (i64, i64) {
    sqlx::query_as("SELECT (SELECT COUNT(*) FROM usage_reservations WHERE key_id = $1), (SELECT COUNT(*) FROM ledger_entries WHERE key_id = $2 AND kind = 'usage')")
        .bind(key_id.to_string()).bind(key_id.to_string()).fetch_one(pool).await.unwrap()
}

async fn exercise(database_url: String) {
    let upstream = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "group-custom-completion", "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "custom works"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        }))).expect(1).mount(&upstream).await;
    let state = AppState::initialize(Config::for_test(database_url.clone()))
        .await
        .unwrap();
    let pool = AnyPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .unwrap();
    let tenant = format!("group-finance-{}", Uuid::now_v7());
    let model = format!("custom-{}", Uuid::now_v7());
    let pepper = state.config.key_pepper.as_bytes();
    let key = state
        .db
        .create_key(key_input(&tenant, "allowed"), pepper)
        .await
        .unwrap();
    let denied = state
        .db
        .create_key(key_input(&tenant, "ungranted"), pepper)
        .await
        .unwrap();
    let account = state.db.create_upstream_account(CreateUpstreamAccountInput {
        tenant_external_id: tenant.clone(), name: "custom".into(), driver: "http-json".into(),
        config: json!({"base_url": upstream.uri(), "network_scope": "private", "stream_usage_contract": "openai-chat-usage-only", "input_token_overhead_ceiling": 0}),
        credential: UpstreamCredential::None, oauth_session_id: None, oauth_driver: None, oauth_refresh_url: None,
    }, pepper).await.unwrap();
    let included = group(&state, &tenant, "dynamic").await;
    members(&state, &pool, &tenant, included, vec![account.id]).await;
    let input = CreateRoutedModelRouteInput {
        tenant_external_id: tenant.clone(),
        public_model: "secure-route-model".into(),
        upstream_model: model.clone(),
        protocol: "openai".into(),
        priority: 0,
        enabled: true,
        upstream_account_ids: vec![],
        included_provider_group_ids: vec![included],
        excluded_provider_group_ids: vec![],
        route_group_ids: vec![],
        route_group_names: vec![],
        granted_credential_ids: vec![key.key_id],
        custom_model_confirmed: true,
    };
    let (route, _) = state
        .db
        .create_routed_model_route(input.clone())
        .await
        .unwrap();
    let body = json!({"model": "secure-route-model", "messages": [{"role": "user", "content": "custom"}], "max_completion_tokens": 8});
    let (status, error) = request(&state, &key.key, "/v1/chat/completions", body.clone()).await;
    assert_eq!(status, StatusCode::FAILED_DEPENDENCY);
    assert_eq!(error["error"]["code"], "unpriced_model");
    assert_eq!(financial_counts(&pool, key.key_id).await, (0, 0));
    assert!(upstream.received_requests().await.unwrap().is_empty());
    state
        .db
        .upsert_model_price(&model, "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let (status, error) = request(&state, &denied.key, "/v1/chat/completions", body.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error["error"]["code"], "forbidden");
    assert_eq!(financial_counts(&pool, denied.key_id).await, (0, 0));
    assert!(upstream.received_requests().await.unwrap().is_empty());
    let (status, response) = request(&state, &key.key, "/v1/chat/completions", body).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["choices"][0]["message"]["content"], "custom works");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if financial_counts(&pool, key.key_id).await == (1, 1) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("one custom request must settle exactly once");
    let settled: (String, i64) =
        sqlx::query_as("SELECT status, actual_micros FROM usage_reservations WHERE key_id = $1")
            .bind(key.key_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(settled, ("settled".into(), 2));
    let charged: i64 = sqlx::query_scalar(
        "SELECT amount_micros FROM ledger_entries WHERE key_id = $1 AND kind = 'usage'",
    )
    .bind(key.key_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(charged, -2);
    let assignment: (String, String) = sqlx::query_as("SELECT upstream_account_id, model_route_id FROM request_records WHERE key_id = $1 AND reservation_id <> ''")
        .bind(key.key_id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(assignment, (account.id.to_string(), route.id.to_string()));
    let sent = upstream.received_requests().await.unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].body_json::<Value>().unwrap()["model"], model);
    upstream.verify().await;

    let codex = state.db.create_upstream_account(CreateUpstreamAccountInput {
        tenant_external_id: tenant.clone(), name: "codex".into(), driver: "openai-codex".into(),
        config: json!({"base_url": "https://chatgpt.com/backend-api/codex", "network_scope": "public", "reservation_token_bounds": {}}),
        credential: UpstreamCredential::OAuth {
            access_token: "test-access".into(), refresh_token: Some("test-refresh".into()), expires_at: Some(i64::MAX),
            header: "authorization".into(), prefix: "Bearer ".into(),
            adapter_state: Some(json!({"schema": "openai-codex-oauth-v1", "account_id": "test-account"})),
            proxy_url: None, proxy_network_scope: None,
        }, oauth_session_id: None, oauth_driver: Some("openai-codex".into()), oauth_refresh_url: None,
    }, pepper).await.unwrap();
    let codex_group = group(&state, &tenant, "codex").await;
    members(&state, &pool, &tenant, codex_group, vec![codex.id]).await;
    let mut codex_input = input;
    codex_input.public_model = "secure-generation-model".into();
    codex_input.included_provider_group_ids = vec![codex_group];
    let (_, routing) = state
        .db
        .create_routed_model_route(codex_input)
        .await
        .unwrap();
    assert_eq!(routing.candidate_upstream_account_ids, vec![codex.id]);
    let (status, _) = request(
        &state,
        &key.key,
        "/v1/responses",
        json!({"model": "secure-generation-model", "input": "no guessed bound", "stream": true}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(financial_counts(&pool, key.key_id).await, (1, 1));
    let config: String =
        sqlx::query_scalar("SELECT config_json FROM upstream_accounts WHERE id = $1")
            .bind(codex.id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&config).unwrap()["reservation_token_bounds"],
        json!({})
    );
    pool.close().await;
}

#[tokio::test]
async fn sqlite_group_custom_wire_price_grants_settlement_and_codex_bound() {
    let directory = tempfile::tempdir().unwrap();
    Box::pin(exercise(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("group-finance.db").display()
    )))
    .await;
}

#[tokio::test]
async fn postgres_group_custom_wire_price_grants_settlement_and_codex_bound() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        assert!(
            std::env::var_os("CI").is_none(),
            "PostgreSQL group-custom CI must not silently skip"
        );
        return;
    };
    Box::pin(exercise(database_url)).await;
}
