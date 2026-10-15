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

async fn wait_for_key_settlement(state: &AppState, key_id: Uuid) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let rows = state.db.list_requests(key_id, 10).await.unwrap();
            if rows.len() == 1 && rows[0].status_code.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
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
                    max_concurrency: 4,
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
