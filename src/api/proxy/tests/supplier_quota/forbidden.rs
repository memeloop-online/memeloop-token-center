use super::*;

#[tokio::test]
async fn delayed_complete403_body_fails_over_only_for_definite_quota() {
    for (code, expected, standby_requests) in [
        ("insufficient_quota", StatusCode::OK, 1),
        ("invalid_api_key", StatusCode::FORBIDDEN, 0),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let upstream = axum::Router::new().fallback(move || async move {
            let body = futures_util::stream::once(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok::<_, std::io::Error>(Bytes::from(
                    serde_json::to_vec(&json!({"error":{"code":code}})).unwrap(),
                ))
            });
            (
                StatusCode::FORBIDDEN,
                [(header::CONTENT_TYPE, "application/json")],
                Body::from_stream(body),
            )
        });
        let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let standby = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(successful_chat_response())
            .expect(standby_requests)
            .mount(&standby)
            .await;
        let fixture = resilient_route_fixture(code, &[(endpoint, 0), (standby.uri(), 10)]).await;
        let response = send_resilient_chat(&fixture, None, false).await;
        assert_eq!(response.status(), expected);
        let body: Value = serde_json::from_slice(
            &to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
                .await
                .unwrap(),
        )
        .unwrap();
        if expected == StatusCode::OK {
            assert_eq!(body["choices"][0]["message"]["content"], "ok");
            assert_eq!(body["usage"]["total_tokens"], 3);
        } else {
            assert_eq!(body["error"]["code"], "authentication_invalid");
        }
        standby.verify().await;
        server.abort();
    }
}

#[tokio::test]
async fn explicit403_quota_fails_over_avoids_failed_model_and_recovers_only_from_real_probe() {
    let exhausted = MockServer::start().await;
    let healthy = MockServer::start().await;
    let unauthorized = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": {"code":"insufficient_user_quota", "type":"new_api_error",
                "message":"用户额度不足, 剩余额度: private-quota-canary"}
        })))
        .expect(1)
        .mount(&exhausted)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(successful_chat_response())
        .expect(2)
        .mount(&healthy)
        .await;
    Mock::given(method("POST"))
        .respond_with(successful_chat_response())
        .expect(0)
        .mount(&unauthorized)
        .await;
    let fixture = resilient_route_fixture(
        "explicit403-quota",
        &[
            (exhausted.uri(), 0),
            (healthy.uri(), 10),
            (unauthorized.uri(), -10),
        ],
    )
    .await;
    let tenant = "resilient-explicit403-quota";
    let account = fixture.accounts[0];
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("DELETE FROM routing_grants WHERE key_id=$1 AND model_route_id IN (SELECT id FROM model_routes WHERE upstream_account_id=$2)")
        .bind(fixture.key_id.to_string()).bind(fixture.accounts[2].to_string())
        .execute(&pool).await.unwrap();
    let context = fixture
        .state
        .db
        .probe_account_context(account)
        .await
        .unwrap();
    let lease = Uuid::now_v7();
    assert!(
        fixture
            .state
            .db
            .claim_upstream_model_catalog_sync(
                account,
                tenant,
                context.credential_generation,
                lease,
            )
            .await
            .unwrap()
    );
    fixture
        .state
        .db
        .replace_upstream_model_catalog(
            account,
            tenant,
            context.credential_generation,
            lease,
            "openai_v1",
            &[crate::db::DiscoveredUpstreamModel {
                model_id: "upstream-0".into(),
                protocol: "openai".into(),
                context_window: None,
                reservation_token_bound: None,
                reservation_bound_source: None,
            }],
        )
        .await
        .unwrap();
    let route_id: String =
        sqlx::query_scalar("SELECT id FROM model_routes WHERE upstream_account_id=$1")
            .bind(account.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    fixture.state.db.plugin_kv_put(
        "upstream-health-probe", &account.to_string(),
        &serde_json::to_vec(&json!({"enabled":true,"billing_key_id":fixture.key_id,"interval_seconds":3600,"model":null,"prompt":"Reply with ok."})).unwrap(),
    ).await.unwrap();
    for _ in 0..2 {
        let response = send_resilient_chat(&fixture, None, false).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], "ok");
        assert_eq!(body["usage"]["total_tokens"], 3);
        assert!(!String::from_utf8_lossy(&bytes).contains("quota-canary"));
        fixture.state.routing_persistence.drain_for_test().await;
    }
    let evidence = sqlx::query("SELECT upstream_model,failure_kind,reset_at FROM upstream_supplier_availability WHERE upstream_account_id=$1")
        .bind(account.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(evidence.get::<String, _>("upstream_model"), "upstream-0");
    assert_eq!(evidence.get::<String, _>("failure_kind"), "quota_exhausted");
    assert_eq!(evidence.get::<Option<i64>, _>("reset_at"), None);
    assert!(
        fixture
            .state
            .db
            .upstream_service_probe(account)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fixture
            .state
            .db
            .supplier_quota_recovery_models(account)
            .await
            .unwrap(),
        vec!["upstream-0"]
    );
    let selection = crate::db::RouteSelectionOptions {
        upstream_account_hint: Some(account),
        avoid_route_account: None,
        selection_seed: Uuid::now_v7(),
    };
    let identity = fixture
        .state
        .db
        .authenticate_key(&fixture.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let candidates = fixture
        .state
        .db
        .list_authorized_upstream_candidates_with_hint(
            fixture.key_id,
            identity.tenant_id,
            &fixture.model,
            "openai",
            selection,
        )
        .await
        .unwrap();
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.account_id)
            .collect::<Vec<_>>(),
        vec![fixture.accounts[1]]
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let requests = fixture
                .state
                .db
                .list_requests(fixture.key_id, 10)
                .await
                .unwrap();
            if requests.len() == 2
                && requests
                    .iter()
                    .all(|request| request.completed_at.is_some())
            {
                for request in requests {
                    assert_eq!(request.status_code, Some(200));
                    assert_eq!(request.upstream_account_id, Some(fixture.accounts[1]));
                    assert!(request.usage.tokens.is_some());
                    assert!(request.billing.cost.is_some());
                }
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let ledger_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger_entries l JOIN request_records r ON l.source=r.reservation_id WHERE r.key_id=$1")
        .bind(fixture.key_id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(ledger_count, 2);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error":{"code":"insufficient_user_quota","message":"用户额度不足"}
        })))
        .expect(1)
        .mount(&exhausted)
        .await;
    let failed_probe = crate::api::upstreams::run_upstream_service_probe(
        &fixture.state,
        account,
        "periodic_recovery",
        Some("upstream-0"),
    )
    .await;
    assert_eq!(failed_probe["status"], "unhealthy", "{failed_probe}");
    assert_eq!(failed_probe["error_code"], "quota_exhausted");
    assert_eq!(failed_probe["upstream_status"], 403);
    assert_eq!(failed_probe["actual_model"], "upstream-0");
    fixture.state.routing_persistence.drain_for_test().await;
    exhausted.verify().await;
    healthy.verify().await;
    exhausted.reset().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(body_partial_json(json!({"model":"upstream-0", "max_output_tokens":16})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"probe403-recovered", "object":"response", "status":"completed", "model":"upstream-0",
            "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}],
            "usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3}
        })))
        .expect(1).mount(&exhausted).await;
    tokio::time::sleep(Duration::from_millis(2)).await;
    let probe = crate::api::upstreams::run_upstream_service_probe(
        &fixture.state,
        account,
        "periodic_recovery",
        Some("upstream-0"),
    )
    .await;
    assert_eq!(probe["status"], "healthy", "{probe}");
    assert_eq!(probe["actual_model"], "upstream-0");
    assert_eq!(probe["upstream_status"], 200);
    assert_eq!(probe["status_origin"], "provider");
    let restored = fixture
        .state
        .db
        .list_authorized_upstream_candidates_with_hint(
            fixture.key_id,
            identity.tenant_id,
            &fixture.model,
            "openai",
            selection,
        )
        .await
        .unwrap();
    assert_eq!(restored.first().unwrap().account_id, account);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(successful_chat_response())
        .expect(1)
        .mount(&exhausted)
        .await;
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    exhausted.verify().await;
    healthy.verify().await;
    unauthorized.verify().await;
    pool.close().await;
}

#[tokio::test]
async fn ordinary403_permission_rejection_is_not_quota_or_replay_permission() {
    let denied = MockServer::start().await;
    let standby = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error":{"code":"invalid_api_key","message":"Invalid API key"}
        })))
        .expect(1)
        .mount(&denied)
        .await;
    Mock::given(method("POST"))
        .respond_with(successful_chat_response())
        .expect(0)
        .mount(&standby)
        .await;
    let fixture =
        resilient_route_fixture("ordinary403", &[(denied.uri(), 0), (standby.uri(), 10)]).await;
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let bytes = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["code"], "authentication_invalid");
    fixture.state.routing_persistence.drain_for_test().await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upstream_supplier_availability")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let kind: String = sqlx::query_scalar("SELECT failure_kind FROM request_upstream_transport_diagnostics WHERE upstream_account_id=$1")
        .bind(fixture.accounts[0].to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(kind, "authentication");
    denied.verify().await;
    standby.verify().await;
    pool.close().await;
}

#[tokio::test]
async fn emitted_stream_keeps_attribution_and_never_replays_known_or_unknown_usage() {
    use futures_util::StreamExt;
    for failed in [false, true] {
        let running = MockServer::start().await;
        let standby = MockServer::start().await;
        let prefix = "data: {\"id\":\"chat-403-stream\",\"object\":\"chat.completion.chunk\",\"model\":\"upstream-0\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"already emitted\"},\"finish_reason\":null}]}\n\n";
        let suffix = if failed {
            "data: {\"error\":{\"code\":\"insufficient_user_quota\",\"message\":\"用户额度不足\"}}\n\n"
        } else {
            concat!(
                "data: {\"id\":\"chat-403-stream\",\"object\":\"chat.completion.chunk\",\"model\":\"upstream-0\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: {\"id\":\"chat-403-stream\",\"object\":\"chat.completion.chunk\",\"model\":\"upstream-0\",\"choices\":[],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1,\"total_tokens\":3}}\n\n",
                "data: [DONE]\n\n"
            )
        };
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(format!("{prefix}{suffix}"), "text/event-stream"),
            )
            .expect(1)
            .mount(&running)
            .await;
        Mock::given(method("POST"))
            .respond_with(successful_chat_response())
            .expect(0)
            .mount(&standby)
            .await;
        let fixture = resilient_route_fixture(
            "stream403-no-replay",
            &[(running.uri(), 0), (standby.uri(), 10)],
        )
        .await;
        let response = send_resilient_chat(&fixture, None, true).await;
        let mut stream = response.into_body().into_data_stream();
        let mut bytes = Vec::new();
        while let Some(Ok(chunk)) = stream.next().await {
            bytes.extend_from_slice(&chunk);
        }
        assert!(String::from_utf8_lossy(&bytes).contains("already emitted"));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let requests = fixture
                    .state
                    .db
                    .list_requests(fixture.key_id, 10)
                    .await
                    .unwrap();
                if requests.len() == 1 && (failed || requests[0].completed_at.is_some()) {
                    assert_eq!(requests[0].upstream_account_id, Some(fixture.accounts[0]));
                    assert_eq!(requests[0].usage.tokens.is_some(), !failed);
                    assert_eq!(requests[0].billing.cost.is_some(), !failed);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        running.verify().await;
        standby.verify().await;
    }
}
