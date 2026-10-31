use super::*;

fn completed_responses(model: &str) -> Value {
    json!({
        "id": "resp_native_transport",
        "object": "response",
        "status": "completed",
        "model": model,
        "output": [{
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{"type":"output_text","text":"native response","annotations":[]}]
        }],
        "usage": {"input_tokens":5,"output_tokens":3,"total_tokens":8}
    })
}

async fn send_official_responses(fixture: &CodexRouteFixture, body: &Value) -> Response {
    send_official_responses_with_key(&fixture.state, &fixture.key, body).await
}

async fn send_official_responses_with_key(state: &AppState, key: &str, body: &Value) -> Response {
    router_for_role(state.clone(), RuntimeRole::Gateway)
        .oneshot(
            Request::post("/v1/responses")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::USER_AGENT, "codex_exec/0.154.0")
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .body(Body::from(serde_json::to_vec(body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}

pub(super) async fn wait_for_key_settlement(state: &AppState, key_id: Uuid) {
    tokio::time::timeout(Duration::from_secs(3), async {
        wait_for_financial_events(state, key_id, 1).await;
    })
    .await
    .unwrap();
}

pub(super) async fn wait_for_financial_events(state: &AppState, key_id: Uuid, expected: usize) {
    let pool = sqlx::AnyPool::connect(&state.config.database_url)
        .await
        .unwrap();
    let owner = Uuid::now_v7();
    loop {
        let rows = state.db.list_requests(key_id, 10).await.unwrap();
        if rows.len() == expected && rows.iter().all(|row| row.completed_at.is_some()) {
            for request_id in state
                .db
                .claim_terminal_projection_tasks(owner, 10)
                .await
                .expect("claim terminal projections")
            {
                assert!(
                    state
                        .db
                        .project_claimed_terminal_projection_task(owner, request_id)
                        .await
                        .unwrap()
                );
            }
            let finished: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM request_events WHERE key_id = $1 AND event_kind = 'finished'",
            )
            .bind(key_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
            if finished == expected as i64 {
                break;
            }
        }
        tokio::task::yield_now().await;
    }
    pool.close().await;
}

#[tokio::test]
async fn explicit_900_second_timeout_streams_past_600_and_settles_once() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let fixture = response_usage_fixture_with_uri_contract_driver_model_and_config(
        "explicit-timeout-stream",
        endpoint,
        0,
        None,
        "http-json",
        "explicit-timeout-model",
        json!({"timeout_seconds":900}),
    )
    .await;
    let completed = completed_responses(&fixture.upstream_model);
    let terminal = format!(
        "event: response.completed\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"type":"response.completed","response":completed})
    );
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let header_end = loop {
            let mut chunk = [0_u8; 4096];
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0);
            request.extend_from_slice(&chunk[..read]);
            if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = std::str::from_utf8(&request[..header_end]).unwrap();
        assert!(headers.starts_with("POST /v1/responses HTTP/1.1\r\n"));
        let body_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        while request.len() < header_end + body_length {
            let mut chunk = [0_u8; 4096];
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0);
            request.extend_from_slice(&chunk[..read]);
        }
        let forwarded: Value =
            serde_json::from_slice(&request[header_end..header_end + body_length]).unwrap();
        assert_eq!(forwarded["stream"], true);
        let progress = b": upstream progress\n\n";
        let content_length = progress.len() * 61 + terminal.len();
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        stream.flush().await.unwrap();
        let started = std::time::Instant::now();
        for _ in 0..61 {
            tokio::time::sleep(Duration::from_secs(10)).await;
            stream.write_all(progress).await.unwrap();
            stream.flush().await.unwrap();
        }
        assert!(started.elapsed() > Duration::from_secs(600));
        stream.write_all(terminal.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "the completed POST must not be replayed"
        );
    });
    let response = send_official_responses(
        &fixture,
        &json!({"model":fixture.model,"input":"explicit timeout","stream":true,
            "max_output_tokens":128}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let rendered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let rendered = String::from_utf8(rendered.to_vec()).unwrap();
    assert!(rendered.contains("response.completed"));
    assert!(rendered.contains("[DONE]"));
    server.await.unwrap();
    wait_for_key_settlement(&fixture.state, fixture.key_id).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status_code, Some(200));
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (5, 3));
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp_native_transport"))
        .await;
}

#[tokio::test]
async fn http_json_native_transports_preserve_codex_multi_agent_wire() {
    for (label, extra_config) in [
        ("default-native-responses", json!({})),
        (
            "explicit-native-responses",
            json!({"responses_transport":"native_responses"}),
        ),
    ] {
        let upstream = MockServer::start().await;
        let fixture = response_usage_fixture_with_uri_contract_driver_model_and_config(
            label,
            upstream.uri(),
            0,
            None,
            "http-json",
            "deepseek-compatible-model",
            extra_config,
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(completed_responses(&fixture.upstream_model)),
            )
            .expect(1)
            .mount(&upstream)
            .await;

        let mut request: Value = serde_json::from_str(include_str!(
            "../../kimi_transport/fixtures/codex-multi-agent-v2.json"
        ))
        .unwrap();
        request["model"] = Value::String(fixture.model.clone());
        request["stream"] = Value::Bool(false);
        request["max_output_tokens"] = json!(128);
        let expected_wire = request.clone();

        let response = send_official_responses(&fixture, &request).await;
        assert_eq!(response.status(), StatusCode::OK);
        let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();

        let received = upstream.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let forwarded: Value = received[0].body_json().unwrap();
        assert_eq!(forwarded, expected_wire);
        upstream.verify().await;
    }
}

#[tokio::test]
async fn http_json_chat_transport_translates_wire_response_and_usage() {
    let upstream = MockServer::start().await;
    let fixture = response_usage_fixture_with_uri_contract_driver_model_and_config(
        "explicit-chat-completions",
        upstream.uri(),
        0,
        Some("openai-chat-usage-only"),
        "http-json",
        "glm-compatible-model",
        json!({"responses_transport":"chat_completions"}),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({
            "model": fixture.upstream_model,
            "stream": false,
            "max_tokens": 128
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"chat_transport",
            "object":"chat.completion",
            "choices":[{
                "index":0,
                "message":{"role":"assistant","content":"translated response"},
                "finish_reason":"stop"
            }],
            "usage":{"prompt_tokens":5,"completion_tokens":3,"total_tokens":8}
        })))
        .expect(1)
        .mount(&upstream)
        .await;

    let mut request: Value = serde_json::from_str(include_str!(
        "../../kimi_transport/fixtures/codex-multi-agent-v2.json"
    ))
    .unwrap();
    request["model"] = Value::String(fixture.model.clone());
    request["stream"] = Value::Bool(false);
    request["max_output_tokens"] = json!(128);

    let response = send_official_responses(&fixture, &request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let response_body: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(response_body["object"], "response");
    assert_eq!(
        response_body["output"][0]["content"][0]["text"],
        "translated response"
    );

    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let forwarded: Value = received[0].body_json().unwrap();
    assert!(
        forwarded["messages"]
            .as_array()
            .is_some_and(|messages| { messages.iter().any(|message| message["role"] == "user") })
    );
    assert!(!forwarded.to_string().contains("encrypted_content"));
    assert!(!forwarded.to_string().contains("\"encrypted\":true"));

    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (5, 3));
    assert_eq!(
        rows[0].usage_basis,
        Some(crate::model::RequestUsageBasis::ProviderReported)
    );
    upstream.verify().await;
}

#[tokio::test]
async fn same_model_accounts_are_explicitly_pinned_to_native_or_chat_transport() {
    let native_upstream = MockServer::start().await;
    let chat_upstream = MockServer::start().await;
    let native = response_usage_fixture_with_uri_contract_driver_model_and_config(
        "dual-account-transport",
        native_upstream.uri(),
        0,
        None,
        "http-json",
        "shared-compatible-model",
        json!({"responses_transport":"native_responses"}),
    )
    .await;
    let tenant = "compatibility-route-dual-account-transport";
    let chat_account = native
        .state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.into(),
                name: "dual-account-chat".into(),
                driver: "http-json".into(),
                config: json!({
                    "base_url": chat_upstream.uri(),
                    "network_scope":"public",
                    "responses_transport":"chat_completions",
                    "stream_usage_contract":"openai-chat-usage-only"
                }),
                credential: UpstreamCredential::ApiKey {
                    value: "chat-account-secret".into(),
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
                },
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            native.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let chat_route = native
        .state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant.into(),
            public_model: native.model.clone(),
            upstream_account_id: chat_account.id,
            upstream_model: native.upstream_model.clone(),
            protocol: "openai".into(),
            priority: 0,
        })
        .await
        .unwrap();
    let chat_key = native
        .state
        .db
        .create_key_with_routing(
            CreateKeyInput {
                tenant_external_id: tenant.into(),
                principal_external_id: "chat-account-client".into(),
                alias: "chat-account-client".into(),
                currency: "USD".into(),
                policy: KeyPolicy {
                    allowed_models: vec![native.model.clone()],
                    max_concurrency: Some(4),
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::ONE,
                idempotency_key: None,
            },
            &[chat_route.id],
            &[],
            native.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header_matcher(
            "authorization",
            "Bearer compatibility-upstream-secret",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(completed_responses(&native.upstream_model)),
        )
        .expect(1)
        .mount(&native_upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header_matcher("authorization", "Bearer chat-account-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"chat_dual_account",
            "object":"chat.completion",
            "choices":[{"index":0,"message":{"role":"assistant","content":"chat response"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":5,"completion_tokens":3,"total_tokens":8}
        })))
        .expect(1)
        .mount(&chat_upstream)
        .await;

    let request = json!({
        "model":native.model.clone(),
        "input":"explicit account transport",
        "stream":false,
        "max_output_tokens":128
    });
    let native_response = send_official_responses(&native, &request).await;
    assert_eq!(native_response.status(), StatusCode::OK);
    let _ = to_bytes(native_response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let chat_response =
        send_official_responses_with_key(&native.state, &chat_key.key, &request).await;
    assert_eq!(chat_response.status(), StatusCode::OK);
    let _ = to_bytes(chat_response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();

    let native_wire = native_upstream.received_requests().await.unwrap();
    let chat_wire = chat_upstream.received_requests().await.unwrap();
    assert_eq!(native_wire[0].url.path(), "/v1/responses");
    assert_eq!(chat_wire[0].url.path(), "/v1/chat/completions");
    let native_forwarded: Value = native_wire[0].body_json().unwrap();
    assert_eq!(native_forwarded, request);
    let converted: Value = chat_wire[0].body_json().unwrap();
    assert_eq!(
        converted["messages"][0]["content"],
        "explicit account transport"
    );
    assert_eq!(converted["max_tokens"], 128);

    wait_for_request_settlement(&native, 1).await;
    wait_for_key_settlement(&native.state, chat_key.key_id).await;
    for key_id in [native.key_id, chat_key.key_id] {
        let rows = native.state.db.list_requests(key_id, 10).await.unwrap();
        assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (5, 3));
        assert_eq!(
            rows[0].usage_basis,
            Some(crate::model::RequestUsageBasis::ProviderReported)
        );
    }
    native_upstream.verify().await;
    chat_upstream.verify().await;
}

async fn multi_agent_chat_limit_fixture(upstream: &MockServer) -> CodexRouteFixture {
    response_usage_fixture_with_uri_contract_driver_model_and_config(
        "multi-agent-chat-limit",
        upstream.uri(),
        0,
        Some("openai-chat-usage-only"),
        "http-json",
        "glm-compatible-model",
        json!({"responses_transport":"chat_completions"}),
    )
    .await
}

fn multi_agent_chat_limit_request(
    fixture: &CodexRouteFixture,
    stream: bool,
    limit: Option<i64>,
) -> Value {
    let mut request: Value = serde_json::from_str(include_str!(
        "../../kimi_transport/fixtures/codex-multi-agent-v2.json"
    ))
    .unwrap();
    request["model"] = json!(fixture.model);
    request["stream"] = json!(stream);
    request.as_object_mut().unwrap().remove("max_output_tokens");
    if let Some(limit) = limit {
        request["max_output_tokens"] = json!(limit);
    }
    request
}

fn assert_multi_agent_chat_limit_wire(forwarded: &Value, limit: Option<i64>) {
    assert_eq!(
        forwarded.get("max_tokens"),
        limit.map(|limit| json!(limit)).as_ref()
    );
    assert!(forwarded.get("max_output_tokens").is_none());
    assert!(forwarded.get("max_completion_tokens").is_none());
    assert!(
        forwarded["messages"]
            .as_array()
            .is_some_and(|messages| messages.iter().any(|message| message["role"] == "user"))
    );
}

#[tokio::test]
async fn http_json_multi_agent_chat_caps_control_buffered_and_streamed_usage() {
    for stream in [false, true] {
        for limit in [Some(4096), Some(5000), None] {
            let upstream = MockServer::start().await;
            let fixture = multi_agent_chat_limit_fixture(&upstream).await;
            let usage = json!({"prompt_tokens":5,"completion_tokens":5000,"total_tokens":5005});
            let template = if stream {
                let chunk = |choices: Value, usage: Value| {
                    json!({
                        "id":"chat_transport", "object":"chat.completion.chunk", "model":fixture.upstream_model,
                        "choices":choices, "usage":usage
                    })
                };
                let chunks = [
                    chunk(
                        json!([{"index":0,"delta":{"role":"assistant","content":"translated response"},"finish_reason":null}]),
                        Value::Null,
                    ),
                    chunk(
                        json!([{"index":0,"delta":{},"finish_reason":"stop"}]),
                        Value::Null,
                    ),
                    chunk(json!([]), usage),
                ];
                let mut body = chunks
                    .iter()
                    .map(|chunk| format!("data: {chunk}\n\n"))
                    .collect::<String>();
                body.push_str("data: [DONE]\n\n");
                ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
            } else {
                ResponseTemplate::new(200).set_body_json(json!({
                    "id":"chat_transport", "object":"chat.completion", "model":fixture.upstream_model,
                    "choices":[{"index":0,"message":{"role":"assistant","content":"translated response"},"finish_reason":"stop"}],
                    "usage":usage
                }))
            };
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(template)
                .expect(1)
                .mount(&upstream)
                .await;
            let request = multi_agent_chat_limit_request(&fixture, stream, limit);
            let response = send_official_responses(&fixture, &request).await;
            let rejected = limit == Some(4096);
            assert_eq!(
                response.status(),
                if rejected && !stream {
                    StatusCode::BAD_GATEWAY
                } else {
                    StatusCode::OK
                }
            );
            let delivered = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
                .await
                .unwrap();
            if stream {
                let body = String::from_utf8(delivered.to_vec()).unwrap();
                assert!(body.contains("response.completed"));
                assert!(body.contains("translated response"));
                assert!(body.contains(r#""output_tokens":5000"#));
            } else {
                let body: Value = serde_json::from_slice(&delivered).unwrap();
                if rejected {
                    assert_eq!(body["error"]["message"], "upstream returned invalid usage");
                } else {
                    assert_eq!(
                        body["output"][0]["content"][0]["text"],
                        "translated response"
                    );
                    assert_eq!(body["usage"]["output_tokens"], 5000);
                }
            }
            if rejected {
                cancellation::assert_single_unknown_receipt(
                    &fixture,
                    502,
                    Some("upstream_invalid_usage"),
                )
                .await;
            } else {
                wait_for_key_settlement(&fixture.state, fixture.key_id).await;
            }
            let rows = fixture
                .state
                .db
                .list_requests(fixture.key_id, 10)
                .await
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].status_code, Some(if rejected { 502 } else { 200 }));
            assert_eq!(
                rows[0].error_code.as_deref(),
                rejected.then_some("upstream_invalid_usage")
            );
            if rejected {
                assert!(rows[0].usage.tokens.is_none());
                assert_eq!(rows[0].billing.cost, None);
            } else {
                assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (5, 5000));
                assert_eq!(
                    rows[0].cost.parse::<Decimal>().unwrap(),
                    Decimal::new(5005, 6)
                );
            }
            let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
            let row = sqlx::query("SELECT r.price_snapshot_json, r.reserved_tokens, r.status, (SELECT COUNT(*) FROM ledger_entries l WHERE l.source = r.id) AS ledgers, (SELECT COUNT(*) FROM request_events e WHERE e.request_id = q.id AND e.event_kind = 'finished') AS events FROM usage_reservations r JOIN request_records q ON q.reservation_id = r.id")
                .fetch_one(&pool).await.unwrap();
            let snapshot: Value =
                serde_json::from_str(&row.get::<String, _>("price_snapshot_json")).unwrap();
            assert_eq!(snapshot.get("output_token_limit"), Some(&json!(limit)));
            assert!(row.get::<i64, _>("reserved_tokens") >= limit.unwrap_or(4096));
            assert_eq!(
                row.get::<String, _>("status"),
                if rejected { "reserved" } else { "settled" }
            );
            assert_eq!(row.get::<i64, _>("ledgers"), if rejected { 0 } else { 1 });
            assert_eq!(row.get::<i64, _>("events"), if rejected { 0 } else { 1 });
            pool.close().await;
            let received = upstream.received_requests().await.unwrap();
            assert_eq!(received.len(), 1);
            assert_multi_agent_chat_limit_wire(&received[0].body_json::<Value>().unwrap(), limit);
            upstream.verify().await;
        }
    }
}

#[tokio::test]
async fn http_json_multi_agent_chat_cap_survives_independent_connection_finish() {
    for limit in [Some(4096), Some(5000), None] {
        let upstream = MockServer::start().await;
        let fixture = multi_agent_chat_limit_fixture(&upstream).await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
            .expect(1)
            .mount(&upstream)
            .await;
        let body = multi_agent_chat_limit_request(&fixture, false, limit);
        let mut request = Box::pin(send_official_responses(&fixture, &body));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    _ = &mut request => panic!("recovery fixture must not return headers"),
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {
                        if upstream.received_requests().await.unwrap().len() == 1 { break; }
                    }
                }
            }
        })
        .await
        .unwrap();
        let received = upstream.received_requests().await.unwrap();
        assert_multi_agent_chat_limit_wire(&received[0].body_json::<Value>().unwrap(), limit);
        let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
        let row = sqlx::query("SELECT r.*, q.id AS request_id, q.tenant_id FROM usage_reservations r JOIN request_records q ON q.reservation_id = r.id")
            .fetch_one(&pool).await.unwrap();
        let request_id = Uuid::parse_str(&row.get::<String, _>("request_id")).unwrap();
        let tenant_id = Uuid::parse_str(&row.get::<String, _>("tenant_id")).unwrap();
        let snapshot: Value =
            serde_json::from_str(&row.get::<String, _>("price_snapshot_json")).unwrap();
        assert_eq!(snapshot.get("output_token_limit"), Some(&json!(limit)));
        let price: crate::model::ModelPrice = serde_json::from_value(snapshot).unwrap();
        let reservation = crate::model::UsageReservation {
            id: Uuid::parse_str(&row.get::<String, _>("id")).unwrap(),
            account_id: fixture.credit_account_id,
            key_id: fixture.key_id,
            enforcement_mode: crate::model::EnforcementMode::from_storage(
                &row.get::<String, _>("enforcement_mode"),
            )
            .unwrap(),
            reserved_micros: row.get("reserved_micros"),
            input_micros_per_million: price.input_micros_per_million,
            output_micros_per_million: price.output_micros_per_million,
            price_tiers: price.tiers,
            rate_window_start: row.get("rate_window_start"),
            reserved_tokens: row.get("reserved_tokens"),
        };
        let recovered = crate::db::Database::connect(&fixture.database_url)
            .await
            .unwrap();
        let response_object = format!("gap://{request_id}/response");
        let finish = FinishProxyRequest {
            terminal_cause: None,
            usage_basis: Some(crate::model::RequestUsageBasis::ProviderReported),
            first_output_ms: None,
            generation_duration_ms: None,
            request_id,
            tenant_id,
            reservation: &reservation,
            input_token_ceiling: reservation.reserved_tokens - limit.unwrap_or(4096),
            output_token_ceiling: limit.unwrap_or(4096),
            requested_service_tier: None,
            status_code: 200,
            duration_ms: 1,
            usage: TokenUsage {
                input_tokens: 5,
                output_tokens: 5000,
                ..TokenUsage::default()
            },
            error_code: None,
            response_object: &response_object,
            routing_session_id: None,
            routing_terminal_observed_at: None,
            conversation: None,
        };
        let rejected = limit == Some(4096);
        drop(request);
        let receipt =
            cancellation::assert_single_unknown_receipt(&fixture, 499, Some("client_cancelled"))
                .await;
        let claim: crate::db::PendingProxySettlementClaim =
            serde_json::from_value(receipt["claim"].clone()).unwrap();
        assert_eq!(claim.request_id, request_id);
        assert_eq!(claim.reservation_id, reservation.id);
        assert!(matches!(
            recovered.finish_proxy_request(finish.clone()).await,
            Err(AppError::Conflict(_))
        ));
        let receipt_id = Uuid::now_v7();
        let mut first_ack = None;
        for _ in 0..2 {
            let result = recovered
                .finish_pending_proxy_settlement(
                    &claim,
                    receipt_id,
                    crate::db::PendingProxySettlementEvidence::ProviderReported,
                    finish.clone(),
                    ProxyRequestUpstreamAttribution::KeepSelected,
                )
                .await;
            if rejected {
                assert!(matches!(result, Err(AppError::Conflict(_))));
            } else {
                let ack = result.unwrap();
                assert_eq!(ack.request_id, request_id);
                assert_eq!(ack.reservation_id, reservation.id);
                assert_eq!(ack.fence, claim.fence);
                assert_eq!(ack.receipt_id, receipt_id);
                assert_eq!(ack.cost_micros, 5005);
                assert!(!ack.receipt_digest.is_empty());
                if let Some(previous) = first_ack.as_ref() {
                    assert_eq!(&ack, previous);
                } else {
                    first_ack = Some(ack);
                }
            }
        }
        if rejected {
            cancellation::assert_unknown_receipt(&fixture, request_id, 499, "client_cancelled", 0)
                .await;
        } else {
            wait_for_key_settlement(&fixture.state, fixture.key_id).await;
        }
        let terminal = sqlx::query("SELECT q.status_code, q.output_tokens, q.cost_micros, (SELECT COUNT(*) FROM ledger_entries l WHERE l.source = q.reservation_id) AS ledgers, (SELECT COUNT(*) FROM request_events e WHERE e.request_id = q.id AND e.event_kind = 'finished') AS events FROM request_records q WHERE q.id = $1")
            .bind(request_id.to_string()).fetch_one(&pool).await.unwrap();
        assert_eq!(
            terminal.get::<Option<i64>, _>("status_code"),
            if rejected { None } else { Some(200) }
        );
        if !rejected {
            assert_eq!(terminal.get::<i64, _>("output_tokens"), 5000);
            assert_eq!(terminal.get::<i64, _>("cost_micros"), 5005);
        }
        assert_eq!(
            terminal.get::<i64, _>("ledgers"),
            if rejected { 0 } else { 1 }
        );
        assert_eq!(
            terminal.get::<i64, _>("events"),
            if rejected { 0 } else { 1 }
        );
        pool.close().await;
        recovered.close().await;
        upstream.verify().await;
    }
}
