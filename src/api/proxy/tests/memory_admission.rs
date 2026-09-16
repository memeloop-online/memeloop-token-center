use super::*;

#[tokio::test]
async fn concurrent_large_streams_dispatch_while_buffered_partition_is_busy() {
    let fixture = std::sync::Arc::new(codex_route_fixture("large-stream-admission").await);
    fixture
        .state
        .db
        .upsert_model_price(&fixture.model, "USD", Decimal::ZERO, Decimal::ZERO)
        .await
        .unwrap();
    let held = fixture.state.proxy_memory_budget.reservation();
    let retained_partition = fixture.state.config.proxy_memory_budget_bytes as usize / 4;
    assert!(held.try_grow(retained_partition, 1));
    assert!(
        held.finalize_request(tokio::time::Instant::now() + Duration::from_secs(1))
            .await
    );
    let dispatched = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let signal = dispatched.clone();
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(move |_: &wiremock::Request| {
            signal.add_permits(1);
            ResponseTemplate::new(200)
                .set_body_raw(completed_codex_sse("done"), "text/event-stream")
                .set_delay(Duration::from_secs(60))
        })
        .expect(2)
        .mount(&upstream)
        .await;
    let mut requests = Vec::new();
    for _ in 0..2 {
        let fixture = fixture.clone();
        let endpoint = upstream.uri();
        requests.push(tokio::spawn(async move {
            send_codex_route_to_endpoint(
                &fixture,
                endpoint,
                "/v1/responses",
                json!({
                    "model": fixture.model,
                    "input": "x".repeat(8 * 1024 * 1024),
                    "stream": true
                }),
            )
            .await
        }));
        // Observe actual dispatch before starting the next ingress. The first
        // upstream deliberately has not returned headers when the second sends.
        tokio::time::timeout(Duration::from_secs(15), dispatched.acquire())
            .await
            .expect("large stream must reach upstream without retained admission")
            .unwrap()
            .forget();
    }
    assert_eq!(
        fixture.state.proxy_memory_budget.snapshot().2,
        retained_partition
    );
    for request in requests {
        assert!(!request.is_finished());
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
    }
    assert_eq!(
        fixture.state.proxy_memory_budget.snapshot().0,
        retained_partition
    );
    drop(held);
    assert_eq!(fixture.state.proxy_memory_budget.snapshot().0, 0);
    upstream.verify().await;
}

#[tokio::test]
async fn executed_response_waits_for_memory_without_replaying_upstream() {
    let fixture = std::sync::Arc::new(codex_route_fixture("response-memory-wait").await);
    let held = fixture.state.proxy_memory_budget.reservation();
    // Leave the complete route-max ingress allowance before its first poll.
    // At upstream execution, consume the now-refunded ingress allowance so response
    // admission deterministically waits, without a timer or oversized payload.
    let ingress_allowance = fixture.state.config.responses_body_max_bytes as usize * 3;
    assert!(held.try_grow(
        fixture.state.config.proxy_memory_budget_bytes as usize - ingress_allowance,
        1,
    ));
    let upstream = MockServer::start().await;
    let response_blocker = fixture.state.proxy_memory_budget.reservation();
    let upstream_blocker = response_blocker.clone();
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(move |_: &wiremock::Request| {
            assert!(upstream_blocker.try_grow(ingress_allowance - 64 * 1024, 1));
            ResponseTemplate::new(200).set_body_raw(
                completed_codex_sse("delivered after memory release"),
                "text/event-stream",
            )
        })
        .expect(1)
        .mount(&upstream)
        .await;
    let endpoint = upstream.uri();
    let owned_fixture = fixture.clone();
    let request = tokio::spawn(async move {
        let payload = serde_json::to_vec(&json!({"model": owned_fixture.model,
            "input": "wait without retry", "stream": false}))
        .unwrap();
        let request = Request::post("/v1/responses")
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", owned_fixture.key),
            )
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_LENGTH, payload.len())
            .body(Body::from(payload))
            .unwrap();
        codex_transport::with_test_endpoint(
            endpoint,
            router_for_role(owned_fixture.state.clone(), RuntimeRole::Gateway).oneshot(request),
        )
        .await
        .unwrap()
    });
    tokio::time::timeout(
        Duration::from_secs(5),
        fixture
            .state
            .proxy_memory_budget
            .wait_for_response_reservation_for_test(),
    )
    .await
    .expect("executed response reaches memory admission");
    assert!(!request.is_finished());
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    drop(held);
    let response = request.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["output"][0]["content"][0]["text"],
        "delivered after memory release"
    );
    drain_completed_response_archive(&fixture).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-codex")).await;
    upstream.verify().await;
}

#[tokio::test]
async fn exhausted_memory_rejects_chunked_request_before_upstream_and_recovers() {
    let fixture = codex_route_fixture("memory-admission-capacity").await;
    let held = fixture.state.proxy_memory_budget.reservation();
    assert!(held.try_grow(fixture.state.config.proxy_memory_budget_bytes as usize, 1));
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("capacity recovered"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let payload = serde_json::to_vec(&json!({
        "model": fixture.model,
        "input": "no allocation without admission",
        "stream": false
    }))
    .unwrap();
    let split = payload.len() / 2;
    let chunks = vec![
        Ok::<_, std::convert::Infallible>(Bytes::copy_from_slice(&payload[..split])),
        Ok(Bytes::copy_from_slice(&payload[split..])),
    ];
    let request = Request::post("/v1/responses")
        .header(header::AUTHORIZATION, format!("Bearer {}", fixture.key))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from_stream(futures_util::stream::iter(chunks)))
        .unwrap();
    assert!(!request.headers().contains_key(header::CONTENT_LENGTH));
    let rejected = codex_transport::with_test_endpoint(
        upstream.uri(),
        router_for_role(fixture.state.clone(), RuntimeRole::Gateway).oneshot(request),
    )
    .await
    .unwrap();
    assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(rejected.headers().contains_key(header::RETRY_AFTER));
    assert!(upstream.received_requests().await.unwrap().is_empty());
    assert!(
        fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap()
            .is_empty()
    );

    drop(rejected);
    drop(held);
    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        serde_json::from_slice(&payload).unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    drain_completed_response_archive(&fixture).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let refs = fixture
        .state
        .db
        .request_archive_refs(fixture.key_id, rows[0].request_id)
        .await
        .unwrap();
    assert_eq!(
        refs.response_archive_state,
        crate::model::RequestArchiveState::Bound
    );
    assert_eq!(
        fixture
            .state
            .archive
            .get(refs.response_object.as_deref().unwrap())
            .await
            .unwrap(),
        delivered
    );
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-codex")).await;
    upstream.verify().await;
}
