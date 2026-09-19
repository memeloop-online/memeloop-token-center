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
    router_for_role(fixture.state.clone(), RuntimeRole::Gateway)
        .oneshot(
            Request::post("/v1/responses")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::USER_AGENT, "codex_exec/0.154.0")
                .header(header::AUTHORIZATION, format!("Bearer {}", fixture.key))
                .body(Body::from(serde_json::to_vec(body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
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
