use super::*;

#[tokio::test]
async fn postgres_source_archive_pool_locked_still_dispatches_and_reaches_eof() {
    let database_url = std::env::var("MTC_TEST_POSTGRES_URL")
        .expect("source R2 PostgreSQL regression must not silently skip");
    let admin = sqlx::PgPool::connect(&database_url).await.unwrap();
    let schema = format!("source_dispatch_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&database_url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let directory = tempfile::tempdir().unwrap();
    let mut config = Config::for_test(isolated.to_string());
    config.responses_request_spool_path = directory.path().join("spool").display().to_string();
    let state = crate::db::Database::with_source_pool_timeout_for_test(
        Duration::from_secs(30),
        AppState::initialize(config),
    )
    .await
    .unwrap();
    for (protocol, endpoint, response_body) in [
        (
            "openai",
            "/v1/chat/completions",
            json!({
                "id":"chat-r2","object":"chat.completion","model":"source-r2",
                "choices":[{"index":0,"message":{"role":"assistant","content":"source R2 delivered"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
            }),
        ),
        (
            "anthropic",
            "/v1/messages",
            json!({
                "id":"msg-r2","type":"message","role":"assistant","model":"source-r2",
                "content":[{"type":"text","text":"source R2 delivered"}],"stop_reason":"end_turn","stop_sequence":null,
                "usage":{"input_tokens":1,"output_tokens":1}
            }),
        ),
        (
            "openai",
            "/v1/responses",
            json!({
                "id":"resp-r2","object":"response","status":"completed","model":"source-r2",
                "output":[{"id":"item-r2","type":"message","role":"assistant","status":"completed",
                    "content":[{"type":"output_text","text":"source R2 delivered","annotations":[]}]}],
                "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
            }),
        ),
    ] {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
            .expect(1)
            .mount(&upstream)
            .await;
        let tenant = Uuid::now_v7().to_string();
        let account = state.db.create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.clone(),
                name: "source-r2".into(),
                driver: "http-json".into(),
                config: json!({"base_url":upstream.uri(),"network_scope":"public","input_token_overhead_ceiling":0}),
                credential: UpstreamCredential::ApiKey {
                    value: "source-r2-test-key".into(),
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
                },
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            state.config.key_pepper.as_bytes(),
        ).await.unwrap();
        let route = state
            .db
            .create_model_route(CreateModelRouteInput {
                tenant_external_id: tenant.clone(),
                public_model: "source-r2".into(),
                upstream_account_id: account.id,
                upstream_model: "source-r2".into(),
                protocol: protocol.into(),
                priority: 0,
            })
            .await
            .unwrap();
        let issued = state
            .db
            .create_key_with_routing(
                CreateKeyInput {
                    tenant_external_id: tenant,
                    principal_external_id: "member".into(),
                    alias: "source-r2".into(),
                    currency: "USD".into(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::TEN,
                    idempotency_key: None,
                },
                &[route.id],
                &[],
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        state
            .db
            .upsert_model_price("source-r2", "USD", Decimal::ONE, Decimal::ONE)
            .await
            .unwrap();
        let held = state.db.hold_gateway_archive_connection_for_test().await;
        let body = if endpoint == "/v1/responses" {
            json!({"model":"source-r2","input":"source R2","max_output_tokens":16,"stream":false})
        } else {
            json!({"model":"source-r2","messages":[{"role":"user","content":"source R2"}],"max_tokens":16,"stream":false})
        };
        let delivered = tokio::time::timeout(Duration::from_secs(5), async {
            let response = router_for_role(state.clone(), RuntimeRole::Gateway)
                .oneshot(
                    Request::post(endpoint)
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(header::AUTHORIZATION, format!("Bearer {}", issued.key))
                        .body(Body::from(serde_json::to_vec(&body).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
                .await
                .unwrap();
            assert_eq!(
                status,
                StatusCode::OK,
                "{endpoint}: {}",
                String::from_utf8_lossy(&delivered)
            );
            delivered
        })
        .await
        .expect("dispatch, first byte and EOF must precede release of the archive connection");
        assert!(String::from_utf8_lossy(&delivered).contains("source R2 delivered"));
        upstream.verify().await;
        assert_eq!(state.db.request_source_write_failures_for_test(), 0);
        drop(held);
        responses_transport::wait_for_key_settlement(&state, issued.key_id).await;
        let requests = state.db.list_requests(issued.key_id, 10).await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].status_code, Some(200));
        state.db.drain_gateway_persistence_for_test().await;
    }
    state.persistence.drain_for_test().await;
    state.db.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
