use super::*;

#[cfg(unix)]
#[tokio::test]
async fn buffered_http_200_sigterm_waits_for_durable_capture_before_exit() {
    const CHILD: &str = "MTC_BUFFERED_SHUTDOWN_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // Isolate the real SIGTERM from other tests in cargo/nextest.
        let output = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "api::proxy::tests::shutdown_durability::buffered_http_200_sigterm_waits_for_durable_capture_before_exit",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("isolated SIGTERM process must exit after durable handoff")
        .unwrap();
        assert!(
            output.status.success(),
            "child failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }

    let fixture = codex_route_fixture("shutdown-buffered").await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("buffered shutdown durability"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let jobs = fixture.state.persistence.hold_jobs_for_test().await;
    let endpoint = upstream.uri();
    let app = router_for_role(fixture.state.clone(), RuntimeRole::Gateway).layer(
        axum::middleware::from_fn(move |request: Request<Body>, next: axum::middleware::Next| {
            let endpoint = endpoint.clone();
            async move { codex_transport::with_test_endpoint(endpoint, next.run(request)).await }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    // Install before sending the signal; the test process uses the same Unix
    // signal and HTTP -> AppState drain sequence as main.
    let mut terminate =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
    let (http_drained, drained) = tokio::sync::oneshot::channel();
    let state = fixture.state.clone();
    let process = tokio::spawn(async move {
        crate::server::serve(listener, app, async move {
            terminate.recv().await.unwrap();
        })
        .await
        .unwrap();
        let mut drain = Box::pin(state.drain_proxy_tasks());
        assert!(
            futures_util::poll!(&mut drain).is_pending(),
            "SIGTERM must stop at the accepted buffered writer"
        );
        http_drained.send(()).unwrap();
        drain.await;
    });
    let request = serde_json::to_vec(&json!({
        "model": fixture.model,
        "input": "buffered shutdown durability",
        "stream": false,
        "max_output_tokens": 16
    }))
    .unwrap();
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        reqwest::Client::builder()
            .no_proxy()
            .http1_only()
            .build()
            .unwrap()
            .post(format!("http://{address}/v1/responses"))
            .bearer_auth(&fixture.key)
            .header("content-type", "application/json")
            .body(request.clone())
            .send(),
    )
    .await
    .expect("buffered HTTP 200 must not wait for capture capacity")
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let request_id =
        Uuid::parse_str(response.headers()[REQUEST_ID_HEADER].to_str().unwrap()).unwrap();
    let delivered = tokio::time::timeout(Duration::from_secs(5), response.bytes())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&delivered).unwrap()["id"],
        "resp-codex"
    );
    assert_eq!(
        fixture.state.persistence.pending_archives_for_test(),
        1,
        "HTTP 200 must already have registered its original request capture"
    );

    // Retire every other shutdown owner first. Only buffered capture is left;
    // a missing persistence drain cannot hide behind another blocked task.
    fixture.state.proxy_lifecycles.wait_for_idle().await;
    fixture.state.pending_terminals.drain().await;
    fixture.state.responses_request_spool.drain_uploads().await;
    fixture.state.db.drain_gateway_persistence().await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM response_archive_spools WHERE request_id = $1",
        )
        .bind(request_id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap(),
        0,
        "capture cannot create a spool while all jobs are held"
    );
    assert!(
        tokio::process::Command::new("kill")
            .args(["-TERM", &std::process::id().to_string()])
            .status()
            .await
            .unwrap()
            .success()
    );
    drained.await.unwrap();
    assert!(
        !process.is_finished(),
        "SIGTERM must retain the buffered writer"
    );
    drop(jobs);
    tokio::time::timeout(Duration::from_secs(10), process)
        .await
        .expect("durable capture must release shutdown")
        .unwrap();

    let receipt = sqlx::query(
        "SELECT p.state, p.receipt_id, p.receipt_digest, p.cost_micros, p.reservation_id, r.tenant_id, r.status_code, NULLIF(r.error_code, '') AS error_code, r.input_tokens, r.output_tokens FROM pending_proxy_settlements p JOIN request_records r ON r.id = p.request_id WHERE p.request_id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(receipt.get::<String, _>("state"), "applied");
    assert!(receipt.get::<Option<String>, _>("receipt_id").is_some());
    assert!(receipt.get::<Option<String>, _>("receipt_digest").is_some());
    assert!(receipt.get::<Option<i64>, _>("cost_micros").is_some());
    assert_eq!(receipt.get::<i64, _>("status_code"), 200);
    assert_eq!(receipt.get::<Option<String>, _>("error_code"), None);
    assert_eq!(receipt.get::<i64, _>("input_tokens"), 3);
    assert_eq!(receipt.get::<i64, _>("output_tokens"), 2);
    let identity = crate::db::ArchiveSpoolIdentity {
        request_id,
        tenant_id: Uuid::parse_str(&receipt.get::<String, _>("tenant_id")).unwrap(),
        reservation_id: Uuid::parse_str(&receipt.get::<String, _>("reservation_id")).unwrap(),
    };
    let original_response = completed_codex_sse("buffered shutdown durability");
    for (side, expected) in [
        ("request", request.as_slice()),
        ("response", original_response.as_bytes()),
    ] {
        let object = fixture
            .state
            .archive
            .raw_capture_reference(identity.tenant_id, request_id, side)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(object.size_bytes, expected.len() as u64);
        assert_eq!(
            object.blake3_digest,
            blake3::hash(expected).to_hex().to_string()
        );
        assert_eq!(
            fixture
                .state
                .archive
                .get_tenant_content(identity.tenant_id, &object, expected.len())
                .await
                .unwrap()
                .as_ref(),
            expected
        );
    }
    let spools: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM response_archive_spools WHERE request_id = $1")
            .bind(request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(spools, 0, "new captures stay in durable CAS after shutdown");
    assert_exactly_once_side_effects(&fixture, request_id, Some("resp-codex")).await;
    let objects =
        sqlx::query("SELECT request_object, response_object FROM request_records WHERE id = $1")
            .bind(request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    for (column, expected) in [
        ("request_object", request.as_slice()),
        ("response_object", original_response.as_bytes()),
    ] {
        let bytes = fixture
            .state
            .archive
            .get(&objects.get::<String, _>(column))
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), expected);
        assert_eq!(blake3::hash(&bytes), blake3::hash(expected));
    }
    upstream.verify().await;
    pool.close().await;
}

#[tokio::test]
async fn shutdown_waits_for_accepted_early_terminal_through_database_outage() {
    let fixture = codex_route_fixture("shutdown-early-terminal").await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE upstream_accounts SET status = 'disabled' WHERE id = $1")
        .bind(fixture.upstream_account_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER pause_early_terminal BEFORE INSERT ON request_record_locators BEGIN SELECT RAISE(ABORT, 'injected early terminal outage'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    let response = router_for_role(fixture.state.clone(), RuntimeRole::Gateway)
        .oneshot(
            Request::post("/v1/responses")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {}", fixture.key))
                .body(Body::from(
                    json!({"model": fixture.model, "input": "early shutdown"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(!response.headers().contains_key("x-mtc-request-recording"));
    let request_id =
        Uuid::parse_str(response.headers()[REQUEST_ID_HEADER].to_str().unwrap()).unwrap();
    to_bytes(response.into_body(), 4096).await.unwrap();
    fixture.state.responses_request_spool.drain_uploads().await;
    fixture.state.db.drain_gateway_persistence().await;
    let mut shutdown = Box::pin(fixture.state.drain_proxy_tasks());
    assert!(futures_util::poll!(&mut shutdown).is_pending());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM request_records WHERE id = $1")
            .bind(request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    sqlx::query("DROP TRIGGER pause_early_terminal")
        .execute(&pool)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), shutdown)
        .await
        .expect("accepted early terminal must commit before shutdown completes");
    let row = sqlx::query(
        "SELECT status_code, error_code, reservation_id, (SELECT COUNT(*) FROM request_events WHERE request_id = $1 AND event_kind = 'finished') AS finished FROM request_records WHERE id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>("status_code"), 503);
    assert_eq!(
        row.get::<String, _>("error_code"),
        "no_authorized_upstream_candidate"
    );
    assert_eq!(row.get::<String, _>("reservation_id"), "");
    assert_eq!(row.get::<i64, _>("finished"), 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM usage_reservations")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    pool.close().await;
}

#[tokio::test]
async fn stopped_http_server_drains_stream_archive_and_receipt_without_blocking_delivery() {
    let original = completed_codex_sse("shutdown durability").into_bytes();
    let (endpoint, release_body, upstream) =
        sse_delivery::gated_sse_upstream(original.clone()).await;
    let fixture =
        response_usage_fixture_with_uri_and_contract("shutdown-durability", endpoint, 256, None)
            .await;
    let (archive_entered, release_archive) =
        crate::response_archive_spool::pause_next_begin_ack_for_test(&fixture.state);
    let receipt_writer = fixture.state.pending_terminals.hold_writer_for_test().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopping) = tokio::sync::oneshot::channel();
    let (http_drained, drained) = tokio::sync::oneshot::channel();
    let state = fixture.state.clone();
    let process = tokio::spawn(async move {
        crate::server::serve(
            listener,
            router_for_role(state.clone(), RuntimeRole::Gateway),
            async { stopping.await.unwrap() },
        )
        .await
        .unwrap();
        http_drained.send(()).unwrap();
        state.drain_proxy_tasks().await;
    });
    let request = serde_json::to_vec(&json!({
        "model": fixture.model,
        "input": "shutdown durability",
        "stream": true,
        "max_output_tokens": 16
    }))
    .unwrap();
    let response = reqwest::Client::builder()
        .no_proxy()
        .http1_only()
        .build()
        .unwrap()
        .post(format!("http://{address}/v1/responses"))
        .bearer_auth(&fixture.key)
        .header("content-type", "application/json")
        .body(request.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let request_id =
        Uuid::parse_str(response.headers()[REQUEST_ID_HEADER].to_str().unwrap()).unwrap();
    let finalization = streaming::finalization_test_gate::install(request_id);
    archive_entered.await.unwrap();
    stop.send(()).unwrap();
    release_body.send(()).unwrap();
    let delivered = tokio::time::timeout(Duration::from_secs(5), response.bytes())
        .await
        .expect("HTTP delivery must not wait for archive or settlement")
        .unwrap();
    assert_eq!(delivered.as_ref(), original.as_slice());
    assert!(String::from_utf8_lossy(&delivered).contains("response.completed"));
    finalization.entered.notified().await;
    drained.await.unwrap();
    assert!(!process.is_finished());
    finalization.release.notify_one();
    tokio::time::timeout(
        Duration::from_secs(5),
        fixture.state.proxy_lifecycles.wait_for_idle(),
    )
    .await
    .unwrap();
    assert!(!process.is_finished(), "archive writer is still owned");
    release_archive.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while fixture
            .state
            .proxy_archive_stream_permits
            .available_permits()
            != 0
        {
            assert!(!process.is_finished());
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shutdown must pass archive drain before waiting for the receipt");
    assert!(!process.is_finished(), "receipt writer is still owned");
    drop(receipt_writer);
    tokio::time::timeout(Duration::from_secs(10), process)
        .await
        .expect("acknowledged background work must allow shutdown")
        .unwrap();
    upstream.await.unwrap();
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let receipt = sqlx::query(
        "SELECT p.state, p.receipt_id, p.receipt_digest, p.cost_micros, p.status_code AS financial_status, p.error_code AS financial_error, o.status_code AS operational_status, o.error_code AS operational_error, r.status_code AS request_status, NULLIF(r.error_code, '') AS request_error, r.input_tokens, r.output_tokens FROM pending_proxy_settlements p JOIN proxy_operational_terminals o ON o.request_id = p.request_id JOIN request_records r ON r.id = p.request_id WHERE p.request_id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(receipt.get::<String, _>("state"), "applied");
    assert!(receipt.get::<Option<String>, _>("receipt_id").is_some());
    assert!(receipt.get::<Option<String>, _>("receipt_digest").is_some());
    assert!(receipt.get::<Option<i64>, _>("cost_micros").is_some());
    for status in ["financial_status", "operational_status", "request_status"] {
        assert_eq!(receipt.get::<i64, _>(status), 200, "{status}");
    }
    for error in ["financial_error", "operational_error", "request_error"] {
        assert_eq!(receipt.get::<Option<String>, _>(error), None, "{error}");
    }
    assert_eq!(receipt.get::<i64, _>("input_tokens"), 3);
    assert_eq!(receipt.get::<i64, _>("output_tokens"), 2);
    let spool =
        sqlx::query("SELECT state, byte_count FROM response_archive_spools WHERE request_id = $1")
            .bind(request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(spool.get::<String, _>("state"), "pending");
    assert_eq!(spool.get::<i64, _>("byte_count"), original.len() as i64);
    assert_exactly_once_side_effects(&fixture, request_id, Some("resp-codex")).await;
    let objects =
        sqlx::query("SELECT request_object, response_object FROM request_records WHERE id = $1")
            .bind(request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    for (column, expected) in [
        ("request_object", request.as_slice()),
        ("response_object", original.as_slice()),
    ] {
        let object = fixture
            .state
            .archive
            .get(&objects.get::<String, _>(column))
            .await
            .unwrap();
        assert_eq!(object.as_ref(), expected);
    }
    pool.close().await;
}

#[tokio::test]
async fn shutdown_preserves_unknown_financial_responsibility_without_zero_settlement() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("{", "application/json"))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = response_usage_fixture("shutdown-unknown", &upstream, 256).await;
    let response = send_response_usage_request(
        &fixture,
        &json!({"model": fixture.model, "input": "unknown", "stream": false, "max_output_tokens": 16}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let request_id =
        Uuid::parse_str(response.headers()[REQUEST_ID_HEADER].to_str().unwrap()).unwrap();
    to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), fixture.state.drain_proxy_tasks())
        .await
        .expect("durable unknown responsibility must not require invented usage");
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let row = sqlx::query(
        "SELECT p.state, p.unknown_reason, p.receipt_id, p.cost_micros, o.status_code, (SELECT COUNT(*) FROM ledger_entries l WHERE l.source = p.reservation_id) AS ledger_count FROM pending_proxy_settlements p JOIN proxy_operational_terminals o ON o.request_id = p.request_id WHERE p.request_id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("state"), "pending");
    assert_eq!(
        row.get::<String, _>("unknown_reason"),
        "authoritative_usage_unavailable"
    );
    assert_eq!(row.get::<Option<String>, _>("receipt_id"), None);
    assert_eq!(row.get::<Option<i64>, _>("cost_micros"), None);
    assert_eq!(row.get::<i64, _>("status_code"), 502);
    assert_eq!(row.get::<i64, _>("ledger_count"), 0);
    pool.close().await;
    upstream.verify().await;
}
