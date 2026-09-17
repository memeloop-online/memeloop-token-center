use super::*;

#[tokio::test]
async fn postgres_archive_admission_wait_does_not_consume_attempt_deadline() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        eprintln!("MTC_TEST_POSTGRES_URL unset; skipping PostgreSQL attempt deadline contract");
        return;
    };
    let nonce = Uuid::new_v4();
    let schema = format!("proxy_attempt_deadline_{}", nonce.simple());
    let application_name = format!("proxy-attempt-deadline-{}", nonce.simple());
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
    let tenant = format!("attempt-deadline-{nonce}");
    let model = format!("attempt-deadline-model-{nonce}");
    let upstream_model = format!("attempt-deadline-upstream-{nonce}");
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
                    access_token: "attempt-deadline-access".to_owned(),
                    refresh_token: Some("attempt-deadline-refresh".to_owned()),
                    expires_at: Some(i64::MAX),
                    header: "authorization".to_owned(),
                    prefix: "Bearer ".to_owned(),
                    adapter_state: Some(json!({
                        "schema": "openai-codex-oauth-v1",
                        "account_id": "attempt-deadline-account"
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
            upstream_model,
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
    sqlx::query(
        "SELECT cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1 FOR UPDATE",
    )
    .execute(&mut *budget_holder)
    .await
    .unwrap();
    let endpoint = upstream.uri();
    let request_state = state.clone();
    let request = tokio::spawn(async move {
        let body = json!({"model": model, "input": "wait for durable admission", "stream": false});
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
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity
                 WHERE application_name = $1 AND wait_event_type = 'Lock'
                   AND query LIKE 'SELECT cipher_bytes%FROM response_archive_spool_budget%FOR UPDATE%'",
            )
            .bind(&application_name)
            .fetch_one(&admin)
            .await
            .unwrap();
            if waiting == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("request admission must wait at the PostgreSQL spool budget barrier");
    assert!(
        upstream.received_requests().await.unwrap().is_empty(),
        "durable admission must complete before upstream dispatch"
    );

    // Advance the request clock past the configured one-second attempt window
    // while PostgreSQL proves admission is still blocked. No wall-clock sleep
    // participates in the ordering assertion.
    tokio::time::pause();
    tokio::time::advance(Duration::from_millis(1001)).await;
    budget_holder.commit().await.unwrap();
    tokio::time::resume();

    let response = tokio::time::timeout(Duration::from_secs(5), request)
        .await
        .expect("request must complete after the admission barrier is released")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["output"][0]["content"][0]["text"],
        "admitted exactly once"
    );
    upstream.verify().await;

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let outstanding: i64 = sqlx::query_scalar(
                "SELECT
                   (SELECT COUNT(*) FROM request_archive_spools
                    WHERE state IN ('capturing', 'pending', 'uploading'))
                   +
                   (SELECT COUNT(*) FROM response_archive_spools
                    WHERE state IN ('capturing', 'pending', 'uploading'))",
            )
            .fetch_one(&holder_pool)
            .await
            .unwrap();
            if outstanding == 0 {
                break;
            }
            crate::response_archive_spool::process_one_for_test(&state).await;
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("bounded PostgreSQL archive drain");

    state.db.close().await;
    holder_pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
