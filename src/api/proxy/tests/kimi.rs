use super::*;

mod buffered_usage;

#[tokio::test]
async fn translated_kimi_clean_eof_and_done_settle_and_archive_once() {
    for with_done in [false, true] {
        let upstream = MockServer::start().await;
        let bridge = MockServer::start().await;
        let fixture = response_usage_fixture("kimi-terminal", &bridge, 0).await;
        fixture
            .state
            .db
            .upsert_model_price_tier(
                &fixture.model,
                "USD",
                "default",
                Decimal::from(2),
                Decimal::ONE,
                Decimal::from(2),
                Decimal::from(3),
                false,
            )
            .await
            .unwrap();
        // Official Kimi Chat streaming example, including its top-level cache
        // count. See docs/kimi-response-failure-diagnostics.md for provenance.
        let documented = include_str!("../../kimi_transport/fixtures/documented-chat-stream.sse");
        let wire = if with_done {
            documented.to_owned()
        } else {
            documented.replace("data: [DONE]\n\n", "")
        };
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(wire, "text/event-stream"))
            .expect(1)
            .mount(&upstream)
            .await;
        let raw = reqwest::Client::new()
            .post(upstream.uri())
            .send()
            .await
            .unwrap();
        let translated = routing::kimi::translate(
            raw,
            crate::api::responses_via_chat::Context::for_kimi(&json!({"model":fixture.model})),
            true,
            crate::provider::SseFramingLimits::default(),
        )
        .unwrap();
        let chunks = translated.bytes_stream().collect::<Vec<_>>().await;
        assert!(chunks.iter().all(Result::is_ok));
        let body = chunks
            .into_iter()
            .flat_map(Result::unwrap)
            .collect::<Vec<_>>();
        let mut capture = ResponsesSseCapture::for_responses();
        capture.push(&body);
        let summary = capture.finish_summary();
        let usage = summary.usage.as_ref().unwrap();
        assert_eq!(
            (
                usage.input_tokens,
                usage.cached_input_tokens,
                usage.output_tokens
            ),
            (7, 12, 13)
        );
        let ResponsesSseOutcome::Completed { response_id } = summary.outcome else {
            panic!("Kimi clean terminal must complete");
        };
        // Feed actual adapter output through the ordinary gateway delivery and
        // settlement fixture, without weakening Kimi's production fixed origin.
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .expect(1)
            .mount(&bridge)
            .await;
        let response = send_response_usage_request(
            &fixture,
            &json!({"model":fixture.model,
            "input":"short fixture request", "stream":true,"max_output_tokens":16}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let delivered = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            String::from_utf8_lossy(&delivered)
                .matches("event: response.completed\n")
                .count(),
            1
        );
        wait_for_request_settlement(&fixture, 1).await;
        let rows = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(rows[0].status_code, Some(200));
        // Request records expose inclusive input, unlike normalized TokenUsage.
        assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (19, 13));
        assert_eq!(rows[0].cached_input_tokens, 12);
        assert_eq!(
            rows[0].usage_basis,
            Some(crate::model::RequestUsageBasis::ProviderReported)
        );
        // Distinct prices prove the cached subset is not charged as ordinary
        // input or counted twice: 7*2 + 12*1 + 13*3 = 65 microdollars.
        assert_eq!(
            rows[0].cost.parse::<Decimal>().unwrap(),
            Decimal::new(65, 6)
        );
        assert_exactly_once_side_effects(&fixture, rows[0].request_id, response_id.as_deref())
            .await;
        drain_completed_response_archive(&fixture).await;
        let refs = fixture
            .state
            .db
            .request_archive_refs(fixture.key_id, rows[0].request_id)
            .await
            .unwrap();
        let locator = refs.response_object.expect("complete archived response");
        assert!(!locator.starts_with("gap://"));
        let archived = fixture.state.archive.get(&locator).await.unwrap();
        assert_eq!(
            String::from_utf8_lossy(&archived)
                .matches("event: response.completed\n")
                .count(),
            1
        );
        upstream.verify().await;
        bridge.verify().await;
    }
}

#[tokio::test]
async fn kimi_translation_clears_length_and_uses_complete_unknown_length_memory_reservation() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"kimi-buffered", "choices":[{"message":{"role":"assistant","content":"translated"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}
        }))).expect(1).mount(&upstream).await;
    let response = reqwest::Client::new()
        .post(upstream.uri())
        .send()
        .await
        .unwrap();
    assert!(response.content_length().is_some());
    let context = crate::api::responses_via_chat::Context::for_kimi(&json!({"model":"kimi"}));
    let translated = routing::kimi::translate(
        response,
        context,
        false,
        crate::provider::SseFramingLimits::default(),
    )
    .unwrap();
    assert!(translated.content_length().is_none());
    let budget = crate::gateway_body::memory::ProxyMemoryBudget::new(
        crate::config::DEFAULT_PROXY_MEMORY_BUDGET_BYTES,
    );
    let memory = budget.reservation();
    let body = read_bounded_upstream(
        translated,
        MAX_PROXY_RESPONSE_BODY,
        &memory,
        Instant::now(),
        false,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["output"][0]["content"][0]["text"],
        "translated"
    );
    assert_eq!(budget.snapshot().0, 192 * 1024 * 1024);
    drop(memory);
    assert_eq!(budget.snapshot().0, 0);
    upstream.verify().await;
}

async fn send_official_codex_responses_request(
    fixture: &CodexRouteFixture,
    body: &Value,
    accept: &'static str,
) -> Response {
    router_for_role(fixture.state.clone(), RuntimeRole::Gateway)
        .oneshot(
            Request::post("/v1/responses")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, accept)
                .header(header::USER_AGENT, "codex_vscode/0.154.0")
                .header(header::AUTHORIZATION, format!("Bearer {}", fixture.key))
                .body(Body::from(serde_json::to_vec(body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn send_official_codex_responses_to_endpoint(
    fixture: &CodexRouteFixture,
    endpoint: String,
    body: &Value,
    accept: &'static str,
    user_agent: &'static str,
) -> Response {
    let originator = user_agent
        .split_once('/')
        .map(|(originator, _)| originator)
        .expect("versioned official Codex user agent");
    let request = Request::post("/v1/responses")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, accept)
        .header(header::USER_AGENT, user_agent)
        .header("originator", originator)
        .header(header::AUTHORIZATION, format!("Bearer {}", fixture.key))
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    codex_transport::with_test_endpoint(
        endpoint,
        router_for_role(fixture.state.clone(), RuntimeRole::Gateway).oneshot(request),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn native_codex_responses_preserves_v1_and_marks_v2_messages_plaintext_on_the_wire() {
    let upstream = MockServer::start().await;
    let fixture = codex_route_fixture("native-collaboration-wire").await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("native collaboration accepted").into_bytes(),
            "text/event-stream",
        ))
        .expect(2)
        .mount(&upstream)
        .await;

    for (source, user_agent) in [
        (
            include_str!("../../kimi_transport/fixtures/codex-multi-agent-v1.json"),
            "codex_vscode/0.154.0",
        ),
        (
            include_str!("../../kimi_transport/fixtures/codex-multi-agent-v2.json"),
            "codex_exec/0.154.0",
        ),
    ] {
        let mut request: Value = serde_json::from_str(source).unwrap();
        request["model"] = Value::String(fixture.model.clone());
        request["stream"] = Value::Bool(true);
        let response = send_official_codex_responses_to_endpoint(
            &fixture,
            upstream.uri(),
            &request,
            "text/event-stream",
            user_agent,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
    }

    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let forwarded_v1: Value = requests[0].body_json().unwrap();
    let expected_v1: Value = serde_json::from_str(include_str!(
        "../../kimi_transport/fixtures/codex-multi-agent-v1.json"
    ))
    .unwrap();
    let v1_namespace = forwarded_v1["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|tool| tool["name"] == "multi_agent_v1"))
        .expect("native wire preserves the real V1 namespace");
    assert_eq!(v1_namespace, &expected_v1["tools"][0]);
    assert!(!v1_namespace.to_string().contains("encrypted"));

    let forwarded: Value = requests[1].body_json().unwrap();
    assert_eq!(
        requests[1].headers["originator"].to_str().unwrap(),
        "codex_exec"
    );
    assert_eq!(
        requests[1].headers[header::USER_AGENT].to_str().unwrap(),
        "codex_exec/0.154.0"
    );
    {
        let namespaces = forwarded["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(
                forwarded["input"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|item| item["type"] == "additional_tools")
                    .flat_map(|item| item["tools"].as_array().into_iter().flatten()),
            )
            .filter(|tool| tool["type"] == "namespace" && tool["name"] == "collaboration")
            .collect::<Vec<_>>();
        assert!(!namespaces.is_empty());
        for namespace in namespaces {
            assert_eq!(namespace["name"], "collaboration");
            for tool in namespace["tools"].as_array().unwrap() {
                let message = tool.pointer("/parameters/properties/message");
                if matches!(
                    tool["name"].as_str(),
                    Some("spawn_agent" | "send_message" | "followup_task")
                ) {
                    assert!(message.is_some_and(|message| message.get("encrypted").is_none()));
                } else if let Some(message) = message {
                    assert!(message.get("encrypted").is_some());
                }
            }
        }
    }
    upstream.verify().await;
}

#[tokio::test]
async fn codex_exec_direct_and_resume_requests_reach_native_upstream_unchanged() {
    let upstream = MockServer::start().await;
    let fixture = codex_route_fixture("native-codex-exec-direct-resume").await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("ordinary request accepted").into_bytes(),
            "text/event-stream",
        ))
        .expect(2)
        .mount(&upstream)
        .await;

    let requests = [
        json!({
            "model": fixture.model,
            "input": [{"type":"message","role":"user","content":[
                {"type":"input_text","text":"direct request"}
            ]}],
            "stream": true
        }),
        json!({
            "model": fixture.model,
            "input": [
                {"type":"message","role":"user","content":[
                    {"type":"input_text","text":"first turn"}
                ]},
                {"type":"message","role":"assistant","content":[
                    {"type":"output_text","text":"first answer"}
                ]},
                {"type":"message","role":"user","content":[
                    {"type":"input_text","text":"resume request"}
                ]}
            ],
            "stream": true
        }),
    ];
    for request in &requests {
        let response = send_official_codex_responses_to_endpoint(
            &fixture,
            upstream.uri(),
            request,
            "text/event-stream",
            "codex_exec/0.154.0",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
    }

    let forwarded = upstream.received_requests().await.unwrap();
    assert_eq!(forwarded.len(), requests.len());
    for (forwarded, original) in forwarded.iter().zip(requests.iter()) {
        let body: Value = forwarded.body_json().unwrap();
        assert_eq!(body["input"], original["input"]);
        assert_eq!(
            forwarded.headers["originator"].to_str().unwrap(),
            "codex_exec"
        );
    }
    upstream.verify().await;
}

#[tokio::test]
async fn fake_glm_via_chat_provider_uses_strict_chat_contract_and_reverse_maps_tools() {
    let upstream = MockServer::start().await;
    let mut fixture = response_usage_fixture_with_uri_contract_and_driver_model(
        "fake-glm-via-chat-bridge",
        upstream.uri(),
        0,
        Some("openai-chat-usage-only"),
        "fake-glm-via-chat",
        "gpt-5.5",
    )
    .await;

    let kimi_capabilities = fixture
        .state
        .providers
        .get(crate::oauth::managed::kimi::PROVIDER_DRIVER)
        .and_then(|provider| provider.codex_model_capabilities.clone())
        .expect("Kimi supplies the shared capability fixture");
    let mut fake_provider = fixture
        .state
        .providers
        .get("http-json")
        .expect("HTTP JSON supplies generic credential and config schemas")
        .clone();
    fake_provider.id = "fake-glm-via-chat".into();
    fake_provider.display_name = "Fake GLM Responses-via-Chat provider".into();
    fake_provider.oauth_adapter = None;
    fake_provider.component_adapter = None;
    fake_provider.generation_adapter = None;
    fake_provider.request_compatibility = crate::provider::RequestCompatibility {
        third_party: true,
        responses_via_chat_v1: true,
        responses_via_chat_dialect: Some(crate::provider::ResponsesViaChatDialect::OpenAiChatV1),
        codex_multi_agent_v2: true,
    };
    let mut strict_chat_capabilities = kimi_capabilities;
    strict_chat_capabilities.supported_reasoning_levels.clear();
    strict_chat_capabilities.default_reasoning_level = None;
    fake_provider.codex_model_capabilities = Some(strict_chat_capabilities);
    fixture.state.providers.extend([fake_provider]).unwrap();

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header_matcher(
            "authorization",
            "Bearer compatibility-upstream-secret",
        ))
        .and(body_partial_json(json!({
            "model": fixture.model,
            "messages": [{"role":"user"}],
            "stream": false,
            "max_completion_tokens": 4096
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "fake-buffered",
            "choices": [{
                "index": 0,
                "message": {"role":"assistant","tool_calls":[{
                    "id":"followup-call",
                    "type":"function",
                    "function":{"name":"collaboration__followup_task","arguments":r#"{"message":"follow up"}"#}
                }]},
                "finish_reason":"tool_calls"
            }],
            "usage": {"prompt_tokens":5,"completion_tokens":3,"total_tokens":8}
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    let buffered = send_official_codex_responses_request(
        &fixture,
        &json!({
            "model": fixture.model,
            "input": "buffered delegated task",
            "tools": [
                {"type":"web_search"},
                {"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"followup_task","parameters":{
                        "type":"object","properties":{
                            "message":{"type":"string","encrypted":{"type":"boolean"}}
                        }
                    }}
                ]}
            ],
            "stream": false
        }),
        "application/json",
    )
    .await;
    assert_eq!(buffered.status(), StatusCode::OK);
    let buffered_body: Value = serde_json::from_slice(
        &to_bytes(buffered.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(buffered_body["output"].as_array().is_some_and(|items| {
        items.iter().any(|item| {
            item["type"] == "function_call"
                && item["name"] == "followup_task"
                && item["namespace"] == "collaboration"
                && item["encrypted_function_args"] == json!([])
        })
    }));

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header_matcher(
            "authorization",
            "Bearer compatibility-upstream-secret",
        ))
        .and(header_matcher("accept", "text/event-stream"))
        .and(body_partial_json(json!({
            "model": fixture.model,
            "messages": [{"role":"user","content":[
                {"type":"text","text":"readable delegated task"}
            ]}],
            "stream": true,
            "stream_options": {"include_usage": true},
            "max_completion_tokens": 4096
        })))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            [
                format!(
                    "data: {}\n\n",
                    json!({"id":"fake-stream","object":"chat.completion.chunk","model":fixture.model,"created":7,"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"spawn-call","function":{"name":"collaboration__spawn_agent","arguments":r#"{"message":"spawn task"}"#}}]},"finish_reason":null}],"usage":null})
                ),
                format!(
                    "data: {}\n\n",
                    json!({"id":"fake-stream","object":"chat.completion.chunk","model":fixture.model,"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":null})
                ),
                format!(
                    "data: {}\n\n",
                    json!({"id":"fake-stream","object":"chat.completion.chunk","model":fixture.model,"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":3,"total_tokens":8}})
                ),
                "data: [DONE]\n\n".into(),
            ]
            .concat(),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let streamed = send_official_codex_responses_request(
        &fixture,
        &json!({
            "model": fixture.model,
            "input": [
                {"type":"additional_tools","tools":[
                    {"type":"web_search"},
                    {"type":"namespace","name":"collaboration","tools":[
                        {"type":"function","name":"spawn_agent","parameters":{
                            "type":"object","properties":{
                                "message":{"type":"string","encrypted":{"type":"boolean"}}
                            }
                        }}
                    ]}
                ]},
                {"type":"compaction","encrypted_content":{"ciphertext":"host-state"}},
                {"type":"reasoning","summary":[],
                    "encrypted_content":{"ciphertext":"host-state"}},
                {"type":"agent_message","author":"/root","recipient":"/worker",
                    "content":[
                        {"type":"input_text","text":"readable delegated task"},
                        {"type":"encrypted_content","encrypted_content":"opaque-ciphertext"}
                    ]}
            ],
            "stream": true
        }),
        "text/event-stream",
    )
    .await;
    assert_eq!(streamed.status(), StatusCode::OK);
    let streamed_body = String::from_utf8(
        to_bytes(streamed.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(streamed_body.contains("response.function_call_arguments.done"));
    assert!(streamed_body.contains("response.completed"));
    assert!(streamed_body.contains(r#""name":"spawn_agent""#));
    assert!(streamed_body.contains(r#""namespace":"collaboration""#));
    assert!(streamed_body.contains(r#""encrypted_function_args":[]"#));
    upstream.verify().await;
    let forwarded = upstream.received_requests().await.unwrap();
    assert_eq!(forwarded.len(), 2);
    for request in forwarded {
        let body: Value = request.body_json().unwrap();
        assert!(!body.to_string().contains("web_search"));
        assert!(body["tools"].as_array().is_some_and(|tools| {
            tools.iter().any(|tool| {
                tool.pointer("/function/name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| name.starts_with("collaboration__"))
            })
        }));
        for tool in body["tools"].as_array().into_iter().flatten() {
            if tool
                .pointer("/function/name")
                .and_then(Value::as_str)
                .is_some_and(|name| name.starts_with("collaboration__"))
            {
                let message = tool
                    .pointer("/function/parameters/properties/message")
                    .expect("collaboration tool keeps its message schema");
                assert!(message.get("encrypted").is_none());
            }
        }
    }
}

#[test]
fn native_kimi_billing_does_not_depend_on_client_usage_opt_in() {
    for body in [
        json!({"stream":true}),
        json!({"stream":true,"stream_options":{"include_usage":false}}),
    ] {
        assert!(requires_strict_openai_chat_usage(
            Protocol::OpenAiChat,
            crate::oauth::managed::kimi::PROVIDER_DRIVER,
            &json!({}),
            &body,
        ));
    }
    assert!(!requires_strict_openai_chat_usage(
        Protocol::AnthropicMessages,
        crate::oauth::managed::kimi::PROVIDER_DRIVER,
        &json!({}),
        &json!({"stream":true}),
    ));
}

#[test]
fn native_kimi_headers_keep_one_bearer_and_preserve_sealed_device_identity() {
    let credential = UpstreamCredential::OAuth {
        access_token: "fixture-access".to_owned(),
        refresh_token: Some("fixture-refresh".to_owned()),
        expires_at: None,
        header: "authorization".to_owned(),
        prefix: "Bearer ".to_owned(),
        adapter_state: Some(json!({
            "schema": "kimi-oauth-v1",
            "device_id": "fixture-existing-device",
            "scope": "coding",
            "token_type": "Bearer"
        })),
        proxy_url: None,
        proxy_network_scope: None,
    };
    let request = credential
        .apply(
            reqwest::Client::new().post(network::upstream_api_url(
                crate::oauth::managed::kimi::BASE_URL,
                Protocol::AnthropicMessages.path(),
            )),
            unix_millis(),
        )
        .unwrap();
    let request = crate::oauth::managed::kimi::apply_headers(request, &credential)
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(
        request.url().as_str(),
        "https://api.kimi.com/coding/v1/messages"
    );
    assert_eq!(request.headers().get_all("authorization").iter().count(), 1);
    assert_eq!(request.headers()["authorization"], "Bearer fixture-access");
    assert_eq!(
        request.headers()["x-msh-device-id"],
        "fixture-existing-device"
    );
}
