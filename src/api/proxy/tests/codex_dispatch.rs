use super::*;

async fn dispatch_route(
    fixture: &CodexRouteFixture,
    account_id: Uuid,
    queued: usize,
    timeout: u64,
) -> ResolvedUpstream {
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let mut config: Value = serde_json::from_str(
        &sqlx::query_scalar::<_, String>("SELECT config_json FROM upstream_accounts WHERE id = $1")
            .bind(account_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap(),
    )
    .unwrap();
    config["transport_policy"] = json!({"dispatch_max_in_flight": 1, "dispatch_max_queued": queued, "dispatch_queue_timeout_millis": timeout});
    sqlx::query("UPDATE upstream_accounts SET config_json = $1 WHERE id = $2")
        .bind(config.to_string())
        .bind(account_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    ResolvedUpstream {
        route_id: fixture.route_id,
        account_id,
        transport_revision: i64::MIN,
        credential_generation: 1,
        driver: codex_transport::DRIVER.to_owned(),
        base_url: codex_transport::BASE_URL.to_owned(),
        config,
        upstream_model: fixture.upstream_model.clone(),
        credential: UpstreamCredential::None,
    }
}

#[tokio::test]
async fn invalid_dispatch_policy_is_bad_request_without_retry_after_or_admission() {
    let direct = crate::codex_clients::DispatchError::InvalidPolicy.response();
    assert_eq!(direct.status(), StatusCode::BAD_REQUEST);
    assert!(!direct.headers().contains_key(header::RETRY_AFTER));
    let fixture = codex_route_fixture("dispatch-invalid-policy").await;
    dispatch_route(&fixture, fixture.upstream_account_id, 1025, 1).await;
    let upstream = MockServer::start().await;
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "hello"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!response.headers().contains_key(header::RETRY_AFTER));
    assert!(
        fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn failover_dispatch_overload_preserves_response_archive_and_releases_old_lane() {
    for (queued, code) in [
        (0, "codex_dispatch_queue_capacity"),
        (1, "codex_dispatch_queue_timeout"),
    ] {
        let fixture =
            std::sync::Arc::new(codex_route_fixture(&format!("dispatch-failover-{queued}")).await);
        let standby = add_codex_standby_route(
            &fixture,
            &format!("codex-route-dispatch-failover-{queued}"),
            "account-456",
        )
        .await;
        let primary_route = dispatch_route(&fixture, fixture.upstream_account_id, 0, 1000).await;
        let standby_route = dispatch_route(&fixture, standby, queued, 1000).await;
        let held = fixture
            .state
            .codex_clients
            .acquire_dispatch(&standby_route, &fixture.state.metrics)
            .await
            .unwrap();
        let upstream = std::sync::Arc::new(MockServer::start().await);
        Mock::given(method("POST"))
            .and(header_matcher("chatgpt-account-id", "account-123"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&upstream)
            .await;
        let request = {
            let (fixture, upstream) = (fixture.clone(), upstream.clone());
            tokio::spawn(async move {
                send_codex_route(
                    &fixture,
                    &upstream,
                    "/v1/responses",
                    json!({"model": fixture.model, "input": "fail over"}),
                )
                .await
            })
        };
        if queued == 1 {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let metrics = fixture
                        .state
                        .metrics
                        .render(&crate::metrics::RuntimeMetrics::default());
                    if metrics.contains(
                        "memeloop_token_center_codex_dispatch_total{outcome=\"queued\"} 1",
                    ) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            // The next lane is still full. The prior lane must already be free,
            // not merely released once the handler returns its terminal error.
            let old_lane = fixture
                .state
                .codex_clients
                .acquire_dispatch(&primary_route, &fixture.state.metrics)
                .await
                .unwrap();
            drop(old_lane);
        }
        let response = request.await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["error"]["type"], "service_overloaded");
        assert_eq!(body["error"]["code"], code);
        let rows = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status_code, Some(503));
        assert!(rows[0].completed_at.is_some());
        assert_eq!(rows[0].error_code.as_deref(), Some(code));
        assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (0, 0));
        let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
        let terminal: String = sqlx::query_scalar("SELECT status FROM usage_reservations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(terminal, "settled");
        let attributed: String =
            sqlx::query_scalar("SELECT upstream_account_id FROM request_records")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(attributed, fixture.upstream_account_id.to_string());
        pool.close().await;
        drain_completed_response_archive(&fixture).await;
        let refs = fixture
            .state
            .db
            .request_archive_refs(fixture.key_id, rows[0].request_id)
            .await
            .unwrap();
        let locator = refs.response_object.as_deref().unwrap();
        let archived: Value = if let Some(body) = locator.strip_prefix("inline-json:") {
            serde_json::from_str(body).unwrap()
        } else {
            serde_json::from_slice(&fixture.state.archive.get(locator).await.unwrap()).unwrap()
        };
        assert_eq!(archived, body);
        assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
        drop(held);
    }
}

#[tokio::test]
async fn admitted_invalid_dispatch_policy_finishes_as_configuration_error_without_retry_after() {
    let fixture = codex_route_fixture("dispatch-invalid-admitted").await;
    let key = fixture
        .state
        .db
        .authenticate_key(&fixture.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let price = fixture
        .state
        .db
        .model_price(&fixture.model, &key.currency)
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let reservation = fixture
        .state
        .db
        .start_proxy_request(StartProxyRequest {
            request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 20,
            output_token_ceiling: 8,
            protocol: "openai",
            model: &fixture.model,
            request_object: &format!("gap://{request_id}/request"),
            upstream_account_id: Some(fixture.upstream_account_id),
            model_route_id: Some(fixture.route_id),
        })
        .await
        .unwrap();
    let request = BufferedRequest {
        state: &fixture.state,
        reservation,
        request_id,
        started: Instant::now(),
        input_token_ceiling: 20,
        output_token_ceiling: 8,
        requested_service_tier: None,
        conversation: None,
        protocol: Protocol::OpenAiResponses,
        tenant_id: key.tenant_id,
        memory: fixture.state.proxy_memory_budget.reservation(),
    };
    let response = lifecycle::finish_dispatch_failure(
        &request,
        crate::codex_clients::DispatchError::InvalidPolicy,
        Some((fixture.upstream_account_id, fixture.route_id)),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(!response.headers().contains_key(header::RETRY_AFTER));
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["error"]["code"], "invalid_transport_policy");
    assert_eq!(body["error"]["type"], "upstream_error");
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(502));
    assert!(rows[0].completed_at.is_some());
    assert_eq!(
        rows[0].error_code.as_deref(),
        Some("invalid_transport_policy")
    );
}

#[tokio::test]
async fn sse_dispatch_permit_survives_headers_and_releases_on_eof_or_cancellation() {
    for cancelled in [false, true] {
        let fixture = codex_route_fixture(&format!("dispatch-sse-{cancelled}")).await;
        let route = dispatch_route(&fixture, fixture.upstream_account_id, 0, 1000).await;
        let (endpoint, release_body, upstream) =
            sse_delivery::gated_sse_upstream(completed_codex_sse("done").into_bytes()).await;
        let response = send_codex_route_to_endpoint(
            &fixture,
            endpoint,
            "/v1/responses",
            json!({"model": fixture.model, "input": "stream", "stream": true}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(matches!(
            fixture
                .state
                .codex_clients
                .acquire_dispatch(&route, &fixture.state.metrics)
                .await,
            Err(crate::codex_clients::DispatchError::Capacity)
        ));
        if cancelled {
            drop(response);
            upstream.abort();
            drop(release_body);
        } else {
            let request_id =
                Uuid::parse_str(response.headers()[REQUEST_ID_HEADER].to_str().unwrap()).unwrap();
            let gate = streaming::finalization_test_gate::install(request_id);
            let delivered = tokio::spawn(async move {
                to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
                    .await
                    .unwrap()
            });
            release_body.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
                .await
                .unwrap();
            assert!(matches!(
                fixture
                    .state
                    .codex_clients
                    .acquire_dispatch(&route, &fixture.state.metrics)
                    .await,
                Err(crate::codex_clients::DispatchError::Capacity)
            ));
            gate.release.notify_one();
            delivered.await.unwrap();
            upstream.await.unwrap();
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let metrics = fixture
                    .state
                    .metrics
                    .render(&crate::metrics::RuntimeMetrics::default());
                if metrics
                    .contains("memeloop_token_center_codex_dispatch_total{outcome=\"released\"} 1")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            fixture
                .state
                .codex_clients
                .acquire_dispatch(&route, &fixture.state.metrics)
                .await
                .is_ok()
        );
    }
}

#[tokio::test]
async fn dispatch_queue_timeout_and_capacity_have_no_durable_admission_side_effects() {
    let fixture = codex_route_fixture("dispatch-preadmission").await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let mut config: Value = serde_json::from_str(
        &sqlx::query_scalar::<_, String>("SELECT config_json FROM upstream_accounts WHERE id = $1")
            .bind(fixture.upstream_account_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap(),
    )
    .unwrap();
    let upstream = MockServer::start().await;
    for (queued, expected) in [
        (1, "codex_dispatch_queue_timeout"),
        (0, "codex_dispatch_queue_capacity"),
    ] {
        config["transport_policy"] = json!({"dispatch_max_in_flight": 1, "dispatch_max_queued": queued, "dispatch_queue_timeout_millis": 1});
        sqlx::query("UPDATE upstream_accounts SET config_json = $1 WHERE id = $2")
            .bind(config.to_string())
            .bind(fixture.upstream_account_id.to_string())
            .execute(&pool)
            .await
            .unwrap();
        let route = ResolvedUpstream {
            route_id: fixture.route_id,
            account_id: fixture.upstream_account_id,
            transport_revision: i64::MIN,
            credential_generation: 1,
            driver: codex_transport::DRIVER.to_owned(),
            base_url: codex_transport::BASE_URL.to_owned(),
            config: config.clone(),
            upstream_model: fixture.upstream_model.clone(),
            credential: UpstreamCredential::None,
        };
        let held = fixture
            .state
            .codex_clients
            .acquire_dispatch(&route, &fixture.state.metrics)
            .await
            .unwrap();
        let response = send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model": fixture.model, "input": "hello"}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], expected);
        drop(held);
        assert!(
            fixture
                .state
                .db
                .list_requests(fixture.key_id, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(archive_file_count(&fixture.archive_path), 0);
        assert!(upstream.received_requests().await.unwrap().is_empty());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM usage_reservations")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM rate_limit_windows")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
    }
    pool.close().await;
}

#[tokio::test]
async fn connect_retry_and_classified_400_replay_hold_one_dispatch_permit() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let fixture = codex_route_fixture("dispatch-retries").await;
    let upstream = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let metrics = fixture.state.metrics.clone();
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            let rendered = metrics.render(&crate::metrics::RuntimeMetrics::default());
            assert!(
                rendered
                    .contains("memeloop_token_center_codex_dispatch_total{outcome=\"admitted\"} 1")
            );
            assert!(
                !rendered
                    .contains("memeloop_token_center_codex_dispatch_total{outcome=\"released\"}")
            );
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(400)
                    .set_body_json(json!({"error": {"type": "temporarily_unavailable"}}))
            } else {
                ResponseTemplate::new(200)
                    .set_body_raw(completed_codex_sse("retried"), "text/event-stream")
            }
        })
        .expect(2)
        .mount(&upstream)
        .await;
    let response = routing::with_test_pre_delivery_connect_failures(
        1,
        send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model": fixture.model, "input": "retry", "stream": false}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let rendered = fixture
        .state
        .metrics
        .render(&crate::metrics::RuntimeMetrics::default());
    assert!(
        rendered.contains("memeloop_token_center_codex_dispatch_total{outcome=\"admitted\"} 1")
    );
    assert!(
        rendered.contains("memeloop_token_center_codex_dispatch_total{outcome=\"released\"} 1")
    );
}
