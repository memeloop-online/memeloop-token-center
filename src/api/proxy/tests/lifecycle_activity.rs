use super::*;

#[tokio::test]
async fn sixty_five_small_streams_keep_activity_and_settle_once() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (releases, mut receivers) = tokio::sync::mpsc::channel(65);
    let server = tokio::spawn(async move {
        let mut owners = Vec::new();
        for _ in 0..65 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (release, wait) = tokio::sync::oneshot::channel::<()>();
            releases.send(release).await.unwrap();
            owners.push(tokio::spawn(async move {
                let mut request = [0_u8; 4096];
                assert!(socket.read(&mut request).await.unwrap() > 0);
                let terminal = completed_codex_sse("small stream");
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    terminal.len()
                );
                socket.write_all(headers.as_bytes()).await.unwrap();
                socket.flush().await.unwrap();
                if wait.await.is_ok() {
                    let _ = socket.write_all(terminal.as_bytes()).await;
                    let _ = socket.shutdown().await;
                }
            }));
        }
        for owner in owners {
            owner.await.unwrap();
        }
    });
    let fixture =
        response_usage_fixture_with_uri_and_contract("sixty-five-lifecycles", endpoint, 0, None)
            .await;
    fixture
        .state
        .db
        .update_key_policy(
            fixture.key_id,
            KeyPolicy {
                max_concurrency: 100,
                requests_per_minute: 1000,
                tokens_per_minute: 10_000_000,
                ..KeyPolicy::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(fixture.state.config.proxy_lifecycle_concurrency, 64);
    let mut streams = Vec::new();
    for expected in 1..=65 {
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            send_response_usage_request(
                &fixture,
                &json!({"model": fixture.model, "input": "small", "stream": true, "max_output_tokens": 16}),
            ),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "stream {expected}");
        streams.push((response.into_body(), receivers.recv().await.unwrap()));
        assert_eq!(fixture.state.proxy_lifecycles.active(), expected);
    }
    let (used, limit, _, _) = fixture.state.proxy_memory_budget.snapshot();
    assert!(used < limit);
    let service = fixture
        .state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "lifecycle-metrics".to_owned(),
                scopes: vec!["metrics:read".to_owned()],
                tenant_external_id: None,
            },
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let metrics = router_for_role(fixture.state.clone(), RuntimeRole::Gateway)
        .oneshot(
            Request::get("/metrics")
                .header(header::AUTHORIZATION, format!("Bearer {}", service.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(metrics.status(), StatusCode::OK);
    let rendered = to_bytes(metrics.into_body(), 1024 * 1024).await.unwrap();
    assert!(String::from_utf8_lossy(&rendered).contains(
        "memeloop_token_center_background_work_items{queue=\"proxy_lifecycles\",state=\"active\"} 65"
    ));
    let (cancelled_body, cancelled_release) = streams.remove(0);
    drop(cancelled_body);
    tokio::time::timeout(Duration::from_secs(5), async {
        while fixture.state.proxy_lifecycles.active() != 64 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(cancelled_release);
    for (body, release) in streams {
        release.send(()).unwrap();
        let bytes = to_bytes(body, 1024 * 1024).await.unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("response.completed"));
    }
    server.await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        fixture.state.proxy_lifecycles.wait_for_idle(),
    )
    .await
    .unwrap();
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let rows = sqlx::query(
        "SELECT COALESCE(q.status_code, o.status_code) AS status_code, q.completed_at, q.status_code AS financial_status, r.status, r.actual_micros, p.state AS pending_state, p.unknown_reason, o.error_code AS operational_error, o.completed_at AS operational_completed, (SELECT COUNT(*) FROM ledger_entries l WHERE l.source = q.reservation_id) AS ledger_count, (SELECT COUNT(*) FROM request_events e WHERE e.request_id = q.id AND e.event_kind = 'finished') AS event_count FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id LEFT JOIN pending_proxy_settlements p ON p.request_id = q.id LEFT JOIN proxy_operational_terminals o ON o.request_id = q.id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 65);
    let mut cancelled = 0;
    for row in rows {
        let status = row.get::<i64, _>("status_code");
        assert!(status == 200 || status == 499, "terminal status {status}");
        cancelled += usize::from(status == 499);
        if status == 499 {
            assert_eq!(row.get::<Option<i64>, _>("completed_at"), None);
            assert_eq!(row.get::<Option<i64>, _>("financial_status"), None);
            assert_eq!(row.get::<String, _>("status"), "reserved");
            assert_eq!(row.get::<Option<i64>, _>("actual_micros"), None);
            assert_eq!(row.get::<String, _>("pending_state"), "pending");
            assert_eq!(
                row.get::<String, _>("unknown_reason"),
                "authoritative_usage_unavailable"
            );
            assert_eq!(
                row.get::<String, _>("operational_error"),
                "client_cancelled"
            );
            assert!(row.get::<Option<i64>, _>("operational_completed").is_some());
            assert_eq!(row.get::<i64, _>("ledger_count"), 0);
            assert_eq!(row.get::<i64, _>("event_count"), 0);
        } else {
            assert!(row.get::<Option<i64>, _>("completed_at").is_some());
            assert_eq!(row.get::<String, _>("status"), "settled");
            assert_eq!(row.get::<i64, _>("ledger_count"), 1);
            assert_eq!(row.get::<i64, _>("event_count"), 1);
        }
    }
    assert_eq!(cancelled, 1);
    assert_eq!(fixture.state.proxy_lifecycles.active(), 0);
    drain_archive_capture(&fixture).await;
    assert_eq!(fixture.state.proxy_memory_budget.snapshot().0, 0);
    pool.close().await;
}

#[tokio::test]
async fn lifecycle_count_does_not_bypass_real_byte_admission() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;
    let fixture = response_usage_fixture("lifecycle-byte-capacity", &upstream, 0).await;
    let held = fixture.state.proxy_memory_budget.reservation();
    assert!(held.try_grow(fixture.state.config.proxy_memory_budget_bytes as usize, 1));
    let response = send_response_usage_request(
        &fixture,
        &json!({"model": fixture.model, "input": "small", "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "1");
    assert_eq!(fixture.state.proxy_lifecycles.active(), 0);
    drop(held);
    assert_eq!(fixture.state.proxy_memory_budget.snapshot().0, 0);
    upstream.verify().await;
}
