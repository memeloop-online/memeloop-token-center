use super::*;

#[tokio::test(start_paused = true)]
async fn buffered_codex_terminal_can_arrive_after_twenty_minutes() {
    let response = UpstreamResponse::Prefetched {
        status: StatusCode::OK,
        headers: HeaderMap::from_iter([(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        )]),
        version: http::Version::HTTP_11,
        content_length: None,
        stream: Box::pin(futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_secs(1500)).await;
            Ok(Bytes::from(completed_codex_sse("late buffered completion")))
        })),
    };
    let budget = crate::gateway_body::memory::ProxyMemoryBudget::new(
        crate::config::DEFAULT_PROXY_MEMORY_BUDGET_BYTES,
    );
    let memory = budget.reservation();
    let started = tokio::time::Instant::now();
    assert!(
        codex_transport::buffer_response(
            response,
            &memory,
            Duration::from_secs(30),
            None,
            crate::provider::SseFramingLimits::default()
        )
        .await
        .is_ok()
    );
    assert_eq!(started.elapsed(), Duration::from_secs(1500));
    drop(memory);
    assert_eq!(budget.snapshot().0, 0);
}

async fn stream_fixture(
    label: &str,
) -> (
    CodexRouteFixture,
    Uuid,
    Response,
    tokio::sync::mpsc::Sender<Result<Bytes, &'static str>>,
) {
    let fixture = response_usage_fixture_with_uri(label, "https://example.test".into(), 0).await;
    let key = fixture
        .state
        .db
        .authenticate_key(&fixture.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let price = fixture
        .state
        .db
        .upsert_model_price(&fixture.upstream_model, "USD", Decimal::ONE, Decimal::ONE)
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
            input_token_ceiling: 10,
            output_token_ceiling: 20,
            protocol: "responses",
            model: &fixture.model,
            request_object: "inline-json:{}",
            upstream_account_id: Some(fixture.upstream_account_id),
            model_route_id: Some(fixture.route_id),
        })
        .await
        .unwrap();
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let response = stream_response(StreamingResponse {
        state: &fixture.state,
        upstream: UpstreamResponse::Prefetched {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            version: http::Version::HTTP_11,
            content_length: None,
            stream: Box::pin(tokio_stream::wrappers::ReceiverStream::new(receiver)),
        },
        status: StatusCode::OK,
        content_type: Some(HeaderValue::from_static("text/event-stream")),
        is_sse: true,
        capture_json_usage: false,
        protocol: Protocol::OpenAiResponses,
        is_codex_route: false,
        is_kimi_route: false,
        codex_retry: CodexRetryTerminalGuard::inactive(),
        codex_chat_model: None,
        codex_chat_include_usage: false,
        upstream_attempt: UpstreamAttemptGuard::new(
            &fixture.state,
            request_id,
            fixture.route_id,
            fixture.upstream_account_id,
            1,
            1,
            UpstreamAttemptAdmission::Healthy { failure_epoch: 0 },
            None,
        ),
        strict_openai_chat_usage: false,
        upstream_activity: fixture.state.metrics.active_upstream("http-json", "proxy"),
        request_id,
        public_model: fixture.model.clone(),
        upstream_account_id: fixture.upstream_account_id,
        credential_generation: 1,
        sse_framing_limits: crate::provider::SseFramingLimits::default(),
        response_headers: HeaderMap::new(),
        buffered_request: BufferedRequest {
            session_preference: None,
            state: &fixture.state,
            reservation,
            request_id,
            memory_admission_wait: Duration::from_secs(30),
            input_token_ceiling: 10,
            output_token_ceiling: 20,
            requested_service_tier: None,
            conversation: None,
            protocol: Protocol::OpenAiResponses,
            tenant_id: key.tenant_id,
            memory: fixture.state.proxy_memory_budget.reservation(),
        },
        proxy_lifecycle_permit: fixture.state.proxy_lifecycles.start(),
        dispatch_permit: None,
    })
    .await
    .unwrap();
    (fixture, request_id, response, sender)
}

async fn assert_terminal(fixture: &CodexRouteFixture, request_id: Uuid, status: i64) {
    tokio::time::timeout(
        Duration::from_secs(5),
        fixture.state.proxy_lifecycles.wait_for_idle(),
    )
    .await
    .unwrap();
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let row = sqlx::query("SELECT q.status_code, r.status, r.actual_micros, (SELECT COUNT(*) FROM ledger_entries l WHERE l.source = q.reservation_id) AS ledgers, (SELECT COUNT(*) FROM request_events e WHERE e.request_id = q.id AND e.event_kind = 'finished') AS events FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id WHERE q.id = $1")
        .bind(request_id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(row.get::<i64, _>("status_code"), status);
    assert_eq!(row.get::<String, _>("status"), "settled");
    assert_eq!(
        row.get::<i64, _>("actual_micros"),
        if status == 200 { 5 } else { 0 }
    );
    assert_eq!(row.get::<i64, _>("ledgers"), 1);
    assert_eq!(row.get::<i64, _>("events"), 1);
    pool.close().await;
}

#[tokio::test]
async fn sustained_stream_past_twenty_minutes_completes_or_cancels_and_settles_once() {
    for cancel in [false, true] {
        let (fixture, request_id, response, sender) =
            stream_fixture(if cancel { "u02-cancel" } else { "u02-complete" }).await;
        tokio::time::pause();
        let keep_clock_paused = tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        });
        let mut body = response.into_body().into_data_stream();
        for _ in 0..25 {
            sender.send(Ok(Bytes::from_static(b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"progress\"}\n\n"))).await.unwrap();
            assert!(body.next().await.unwrap().is_ok());
            tokio::time::advance(Duration::from_secs(60)).await;
            assert_eq!(fixture.state.proxy_lifecycles.active(), 1);
        }
        tokio::time::resume();
        keep_clock_paused.abort();
        if cancel {
            drop(body);
            assert_terminal(&fixture, request_id, 499).await;
        } else {
            sender
                .send(Ok(Bytes::from(completed_codex_sse(
                    "completed after 25 minutes",
                ))))
                .await
                .unwrap();
            drop(sender);
            let mut wire = Vec::new();
            while let Some(chunk) = body.next().await {
                wire.extend_from_slice(&chunk.unwrap());
            }
            assert!(
                String::from_utf8(wire)
                    .unwrap()
                    .contains("response.completed")
            );
            assert_terminal(&fixture, request_id, 200).await;
        }
    }
}

#[tokio::test]
async fn quiet_stream_has_no_total_cutoff_and_terminal_settlement_keeps_thirty_seconds() {
    let (fixture, request_id, response, sender) = stream_fixture("u02-finalization").await;
    let gate = streaming::finalization_test_gate::install(request_id);
    tokio::time::pause();
    let keep_clock_paused = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(1500)).await;
    assert_eq!(fixture.state.proxy_lifecycles.active(), 1);
    sender
        .send(Ok(Bytes::from(completed_codex_sse("quiet completion"))))
        .await
        .unwrap();
    drop(sender);
    let wire = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(
        String::from_utf8(wire.to_vec())
            .unwrap()
            .contains("response.completed")
    );
    gate.entered.notified().await;
    tokio::time::advance(Duration::from_secs(29)).await;
    assert_eq!(fixture.state.proxy_lifecycles.active(), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::time::resume();
    keep_clock_paused.abort();
    assert_terminal(&fixture, request_id, 504).await;
}
