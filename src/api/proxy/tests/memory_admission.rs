use super::*;

#[tokio::test]
async fn source_backed_conversation_releases_all_raw_request_permits() {
    use axum::body::Body;

    let directory = tempfile::tempdir().unwrap();
    let admission = crate::gateway_body::request_spool::RequestSpoolAdmission::new(
        directory.path().to_owned(),
        1024 * 1024,
    );
    let source = Bytes::from_static(br#"{"model":"gpt-5.6-sol","input":"source-backed"}"#);
    let spool = std::sync::Arc::new(
        admission
            .capture(Body::from(source.clone()), source.len(), Some(source.len()))
            .await
            .unwrap(),
    );
    let budget = crate::gateway_body::memory::ProxyMemoryBudget::new(1024 * 1024);
    let blocker = budget.reservation();
    assert!(blocker.try_grow(1024 * 1024, 1));
    let memory = budget.reservation();
    let conversation = std::sync::Arc::new(ProxyConversation {
        key: crate::model::AuthenticatedKey {
            key_id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            principal_id: Uuid::nil(),
            account_id: Uuid::nil(),
            alias: "test".to_owned(),
            currency: "USD".to_owned(),
            credential_generation: 0,
            policy: KeyPolicy::default(),
        },
        request_body: ConversationBody::Spool(spool),
        hints: crate::conversation::ConversationHints::default(),
        client_name: None,
        projection_admission: std::sync::Mutex::new(ConversationProjectionAdmission::Deferred),
    });
    let projected = {
        let conversation = conversation.clone();
        let memory = memory.clone();
        tokio::spawn(async move {
            let projection = conversation
                .project(
                    &memory,
                    tokio::time::Instant::now() + Duration::from_secs(5),
                )
                .await?;
            Ok::<_, AppError>(projection.request_json["model"].clone())
        })
    };
    budget.wait_for_projection_reservation_for_test().await;
    assert_eq!(admission.read_count_for_test(), 0);
    drop(blocker);
    assert_eq!(projected.await.unwrap().unwrap(), json!("gpt-5.6-sol"));
    assert_eq!(admission.read_count_for_test(), 1);
    assert_eq!(budget.snapshot().0, 0);

    let large_source = Bytes::from(format!(
        r#"{{"model":"gpt-5.6-sol","input":"{}"}}"#,
        "x".repeat(32 * 1024)
    ));
    let combined_spool = std::sync::Arc::new(
        admission
            .capture(
                Body::from(large_source.clone()),
                large_source.len(),
                Some(large_source.len()),
            )
            .await
            .unwrap(),
    );
    let combined_conversation = ProxyConversation {
        key: conversation.key.clone(),
        request_body: ConversationBody::Spool(combined_spool),
        hints: crate::conversation::ConversationHints::default(),
        client_name: None,
        projection_admission: std::sync::Mutex::new(ConversationProjectionAdmission::Deferred),
    };
    let combined_budget = crate::gateway_body::memory::ProxyMemoryBudget::new(256 * 1024);
    let combined_memory = combined_budget.reservation();
    assert!(
        !combined_conversation
            .reserve_for_buffered_response(
                &combined_memory,
                64 * 1024,
                tokio::time::Instant::now(),
            )
            .await
    );
    assert_eq!(combined_budget.snapshot().0, 0);

    let invalid = std::sync::Arc::new(
        admission
            .capture(Body::from(Bytes::from_static(b"{")), 1, Some(1))
            .await
            .unwrap(),
    );
    let invalid_conversation = ProxyConversation {
        key: conversation.key.clone(),
        request_body: ConversationBody::Spool(invalid),
        hints: crate::conversation::ConversationHints::default(),
        client_name: None,
        projection_admission: std::sync::Mutex::new(ConversationProjectionAdmission::Deferred),
    };
    assert!(
        invalid_conversation
            .project(
                &memory,
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .is_err()
    );
    assert_eq!(admission.read_count_for_test(), 2);
    assert_eq!(budget.snapshot().0, 0);
}

#[tokio::test]
async fn concurrent_large_streams_dispatch_while_buffered_partition_is_busy() {
    let fixture = std::sync::Arc::new(codex_route_fixture("large-stream-admission").await);
    // The gateway conservatively reserves input bytes as tokens. Admit both
    // 8 MiB requests through the credential's real TPM and prepaid balance so
    // this test reaches the memory partition behavior it intends to exercise.
    fixture
        .state
        .db
        .update_key_policy(
            fixture.key_id,
            KeyPolicy {
                tokens_per_minute: 32 * 1024 * 1024,
                ..KeyPolicy::default()
            },
        )
        .await
        .unwrap();
    fixture
        .state
        .db
        .grant(
            fixture.credit_account_id,
            Decimal::from(32),
            "large stream memory admission fixture",
            "large-stream-memory-admission-balance",
        )
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
        let mut request = tokio::spawn(async move {
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
        });
        // Observe actual dispatch before starting the next ingress. A local
        // admission error is ready first and must fail with its response rather
        // than being misreported as a dispatch timeout. The upstream delays
        // headers, so a dispatched request remains active while the next large
        // stream enters the gateway.
        tokio::select! {
            biased;
            response = &mut request => {
                let response = response.expect("large stream request task");
                let status = response.status();
                let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
                    .await
                    .expect("early large stream response body");
                panic!(
                    "large stream returned before upstream dispatch: status={status}, body={}",
                    String::from_utf8_lossy(&body)
                );
            }
            permit = dispatched.acquire() => {
                permit.expect("dispatch semaphore remains open").forget();
            }
            _ = tokio::time::sleep(Duration::from_secs(15)) => {
                panic!("large stream did not reach upstream");
            }
        }
        requests.push(request);
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
