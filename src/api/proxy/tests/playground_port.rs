use super::*;

#[tokio::test]
async fn native_codex_refused_connection_settles_zero_once() {
    let fixture = codex_route_fixture("native-connect-not-delivered").await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let response = send_codex_route_to_endpoint(
        &fixture,
        endpoint,
        "/v1/responses",
        json!({"model": fixture.model, "input": "connection was not established", "stream": false}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let request_id =
        Uuid::parse_str(response.headers()[REQUEST_ID_HEADER].to_str().unwrap()).unwrap();
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(
        std::str::from_utf8(&body)
            .unwrap()
            .contains("upstream_connect_failed")
    );
    wait_for_request_settlement(&fixture, 1).await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let zero = sqlx::query("SELECT r.cost_micros, u.status, u.actual_micros, p.state, p.unknown_reason, (SELECT COUNT(*) FROM ledger_entries l WHERE l.source = r.reservation_id AND l.kind = 'usage' AND l.amount_micros = 0) AS settlements FROM request_records r JOIN usage_reservations u ON u.id = r.reservation_id JOIN pending_proxy_settlements p ON p.request_id = r.id WHERE r.id = $1")
        .bind(request_id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(zero.get::<i64, _>("cost_micros"), 0);
    assert_eq!(zero.get::<String, _>("status"), "settled");
    assert_eq!(zero.get::<Option<i64>, _>("actual_micros"), Some(0));
    assert_eq!(zero.get::<String, _>("state"), "applied");
    assert_eq!(zero.get::<Option<String>, _>("unknown_reason"), None);
    assert_eq!(zero.get::<i64, _>("settlements"), 1);
    pool.close().await;
}

#[tokio::test]
async fn cancellation_after_connect_failover_keeps_dispatched_candidate_finance_unknown() {
    use tokio::io::AsyncReadExt as _;
    let refused = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let refused_uri = format!("http://{}", refused.local_addr().unwrap());
    drop(refused);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dispatched_uri = format!("http://{}", listener.local_addr().unwrap());
    let (received_tx, received_rx) = tokio::sync::oneshot::channel();
    let accepted = tokio::spawn(async move {
        let (mut connection, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        loop {
            let mut chunk = [0u8; 4096];
            let count = connection.read(&mut chunk).await.unwrap();
            assert!(count > 0, "candidate must receive the complete request");
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                let headers = std::str::from_utf8(&bytes[..end]).unwrap();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .expect("buffered request has a content length");
                if bytes.len() >= end + 4 + length {
                    let body = &bytes[end + 4..end + 4 + length];
                    assert!(
                        std::str::from_utf8(body)
                            .unwrap()
                            .contains("cancel after delivery")
                    );
                    received_tx.send(()).unwrap();
                    std::future::pending::<()>().await;
                    drop(connection);
                    return;
                }
            }
        }
    });
    let fixture = resilient_route_fixture(
        "cancel-after-connect-failover",
        &[(refused_uri, 0), (dispatched_uri, 10)],
    )
    .await;
    let original = serde_json::to_vec(&json!({"model": fixture.model, "messages": [{"role": "user", "content": "cancel after delivery"}], "stream": false})).unwrap();
    let request = Request::post("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {}", fixture.key))
        .body(Body::from(original.clone()))
        .unwrap();
    let dispatched =
        tokio::spawn(router_for_role(fixture.state.clone(), RuntimeRole::Gateway).oneshot(request));
    tokio::time::timeout(Duration::from_secs(3), received_rx)
        .await
        .expect("second candidate must receive the body")
        .unwrap();
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let request_id = rows[0].request_id;
    dispatched.abort();
    assert!(dispatched.await.unwrap_err().is_cancelled());
    cancellation::assert_unknown_request_state(
        &fixture.state,
        &fixture.database_url,
        fixture.key_id,
        request_id,
        0,
    )
    .await;
    fixture.state.responses_request_spool.drain_uploads().await;
    fixture.state.db.drain_gateway_persistence_for_test().await;
    let refs = fixture
        .state
        .db
        .request_archive_refs(fixture.key_id, request_id)
        .await
        .unwrap();
    let captured = fixture
        .state
        .archive
        .get(&refs.request_object)
        .await
        .unwrap();
    assert_eq!(captured.as_ref(), original.as_slice());
    assert_eq!(blake3::hash(&captured), blake3::hash(&original));
    accepted.abort();
    assert!(accepted.await.unwrap_err().is_cancelled());
}
