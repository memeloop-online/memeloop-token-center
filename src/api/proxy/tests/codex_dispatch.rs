use super::*;

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
