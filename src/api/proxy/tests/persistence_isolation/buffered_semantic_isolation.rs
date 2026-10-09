use super::*;

#[tokio::test]
#[ignore = "requires the configured CI PostgreSQL service"]
async fn postgres_buffered_delivery_does_not_wait_for_semantic_content_locks() {
    let database_url = std::env::var("MTC_TEST_POSTGRES_URL")
        .expect("PostgreSQL contract requires MTC_TEST_POSTGRES_URL");
    let nonce = Uuid::new_v4();
    let schema = format!("buffered_semantic_{}", nonce.simple());
    let application_name = format!("buffered-semantic-{}", nonce.simple());
    let admin = sqlx::PgPool::connect(&database_url).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated_url = url::Url::parse(&database_url).unwrap();
    isolated_url.query_pairs_mut().append_pair(
        "options",
        &format!("-csearch_path={schema} -capplication_name={application_name}"),
    );
    let directory = tempfile::tempdir().unwrap();
    let mut config = Config::for_test(isolated_url.to_string());
    config.archive_backend = ArchiveBackend::Filesystem;
    config.archive_path = Some(directory.path().join("archive").display().to_string());
    let state = AppState::initialize(config).await.unwrap();
    let tenant = format!("semantic-isolation-{nonce}");
    let model = format!("semantic-isolation-model-{nonce}");
    let upstream_model = format!("semantic-isolation-upstream-{nonce}");
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.clone(),
                name: "deadline-after-admission".to_owned(),
                driver: codex_transport::DRIVER.to_owned(),
                config: json!({
                    "base_url": codex_transport::BASE_URL,
                    "network_scope": "public",
                    "reservation_token_bounds": {upstream_model.clone(): 64},
                    "transport_policy": {
                        "version": 1,
                        "candidate_attempts": 1,
                        "failover_deadline_millis": 1000
                    }
                }),
                credential: UpstreamCredential::OAuth {
                    access_token: "semantic-isolation-access".to_owned(),
                    refresh_token: Some("semantic-isolation-refresh".to_owned()),
                    expires_at: Some(i64::MAX),
                    header: "authorization".to_owned(),
                    prefix: "Bearer ".to_owned(),
                    adapter_state: Some(json!({
                        "schema": "openai-codex-oauth-v1",
                        "account_id": "semantic-isolation-account"
                    })),
                    proxy_url: None,
                    proxy_network_scope: None,
                },
                oauth_session_id: None,
                oauth_driver: Some(codex_transport::DRIVER.to_owned()),
                oauth_refresh_url: Some(crate::oauth::managed::codex::TOKEN_ENDPOINT.to_owned()),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let route = state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant.clone(),
            public_model: model.clone(),
            upstream_account_id: account.id,
            upstream_model: upstream_model.clone(),
            protocol: "openai".to_owned(),
            priority: 0,
        })
        .await
        .unwrap();
    let issued = state
        .db
        .create_key_with_routing(
            CreateKeyInput {
                tenant_external_id: tenant,
                principal_external_id: "member".to_owned(),
                alias: "deadline-after-admission".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    allowed_models: vec![model.clone()],
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::ONE,
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
        .upsert_model_price(&model, "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    state
        .db
        .upsert_model_price(&upstream_model, "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("admitted exactly once"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;

    let holder_pool = sqlx::PgPool::connect(isolated_url.as_str()).await.unwrap();
    let mut budget_holder = holder_pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE semantic_atoms, context_nodes IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *budget_holder)
        .await
        .unwrap();
    let endpoint = upstream.uri();
    let request_state = state.clone();
    let request = tokio::spawn(async move {
        let body = json!({"model": model, "input": "durable semantic recovery", "stream": false});
        let request = Request::post("/v1/responses")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, format!("Bearer {}", issued.key))
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        codex_transport::with_test_endpoint(
            endpoint,
            router_for_role(request_state, RuntimeRole::Gateway).oneshot(request),
        )
        .await
        .unwrap()
    });

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if upstream.received_requests().await.unwrap().len() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("upstream must receive the request while the semantic content is still locked");

    let response = tokio::time::timeout(Duration::from_secs(5), request)
        .await
        .expect("request must complete while the semantic content lock is still held")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["output"][0]["content"][0]["text"],
        "admitted exactly once"
    );
    let row =
        sqlx::query("SELECT request_id, tenant_id, key_id FROM conversation_semantic_payloads")
            .fetch_one(&holder_pool)
            .await
            .unwrap();
    let request_id: String = row.get("request_id");
    let request_id = Uuid::parse_str(&request_id).unwrap();
    let charges: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM ledger_entries WHERE kind = 'usage'")
            .fetch_one(&holder_pool)
            .await
            .unwrap();
    assert_eq!(charges, 1);
    budget_holder.commit().await.unwrap();
    upstream.verify().await;
    let terminal_owner = Uuid::now_v7();
    assert!(
        state
            .db
            .claim_terminal_projection_tasks(terminal_owner, 32)
            .await
            .unwrap()
            .contains(&request_id)
    );
    assert!(
        state
            .db
            .project_claimed_terminal_projection_task(terminal_owner, request_id)
            .await
            .unwrap()
    );
    let owner = Uuid::now_v7();
    assert!(
        state
            .db
            .claim_conversation_projection_tasks(owner, 32)
            .await
            .unwrap()
            .iter()
            .any(|task| task.request_id == request_id)
    );
    assert!(
        state
            .db
            .project_claimed_conversation_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    assert!(
        !state
            .db
            .project_claimed_conversation_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    let counts = sqlx::query("SELECT (SELECT COUNT(*) FROM semantic_atoms) AS atoms, (SELECT COUNT(*) FROM context_nodes) AS nodes, (SELECT COUNT(*) FROM conversation_semantic_payloads) AS payloads, (SELECT COUNT(*) FROM ledger_entries WHERE kind = 'usage') AS charges")
        .fetch_one(&holder_pool).await.unwrap();
    assert!(counts.get::<i64, _>("atoms") > 0);
    assert!(counts.get::<i64, _>("nodes") > 0);
    assert_eq!(counts.get::<i64, _>("payloads"), 0);
    assert_eq!(counts.get::<i64, _>("charges"), 1);
    state.db.drain_gateway_persistence_for_test().await;
    state.persistence.drain_for_test().await;

    state.db.close().await;
    holder_pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
