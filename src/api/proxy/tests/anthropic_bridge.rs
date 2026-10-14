use super::*;

async fn fixture(label: &str, upstream: &MockServer) -> CodexRouteFixture {
    response_usage_fixture_with_uri_contract_driver_model_and_credential(
        label,
        upstream.uri(),
        0,
        None,
        crate::oauth::claude::PROVIDER_DRIVER,
        "claude-sonnet-route",
        UpstreamCredential::OAuth {
            access_token: "claude-oauth-access-token".to_owned(),
            refresh_token: Some("claude-oauth-refresh-token".to_owned()),
            expires_at: Some(i64::MAX),
            header: "authorization".to_owned(),
            prefix: "Bearer ".to_owned(),
            adapter_state: Some(json!({"schema":"anthropic-claude-oauth-v1"})),
            proxy_url: None,
            proxy_network_scope: None,
        },
    )
    .await
}

#[tokio::test]
async fn contributed_provider_responses_use_declared_messages_headers() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header_matcher("anthropic-version", "2023-06-01"))
        .and(header_matcher("anthropic-beta", "subscription-v1"))
        .and(header_matcher(
            "authorization",
            "Bearer fixture-oauth-token",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"msg_fixture","type":"message","role":"assistant","model":"fixture-upstream",
            "content":[{"type":"text","text":"fixture response"}],"stop_reason":"end_turn",
            "usage":{"input_tokens":4,"output_tokens":2}
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    let driver = "fixture-messages-subscription";
    let mut fixture = response_usage_fixture_with_uri_contract_driver_model_and_credential(
        "declared-messages-headers",
        upstream.uri(),
        0,
        None,
        driver,
        "fixture-messages-model",
        UpstreamCredential::OAuth {
            access_token: "fixture-oauth-token".into(),
            refresh_token: None,
            expires_at: Some(i64::MAX),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: None,
            proxy_url: None,
            proxy_network_scope: None,
        },
    )
    .await;
    let mut provider = fixture
        .state
        .providers
        .get(crate::oauth::claude::PROVIDER_DRIVER)
        .unwrap()
        .clone();
    provider.id = driver.into();
    provider.oauth_adapter = None;
    provider.request_compatibility.anthropic_messages_headers =
        crate::provider::FixedProtocolHeaders {
            anthropic_version: Some("2023-06-01".into()),
            anthropic_beta: vec!["subscription-v1".into()],
        };
    fixture.state.providers.extend([provider]).unwrap();
    let response = send_response_usage_request(
        &fixture,
        &json!({
            "model":fixture.model,"input":"fixture prompt","stream":false,"max_output_tokens":64
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].headers.get_all("anthropic-beta").iter().count(),
        1
    );
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["model"], fixture.upstream_model);
    assert!(body["messages"].is_array());
    upstream.verify().await;
}

#[tokio::test]
async fn responses_route_dispatches_anthropic_messages_and_restores_buffered_contract() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header_matcher("anthropic-version", "2023-06-01"))
        .and(header_matcher(
            "anthropic-beta",
            crate::oauth::claude::OAUTH_BETA_HEADER,
        ))
        .and(header_matcher(
            "authorization",
            "Bearer claude-oauth-access-token",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"msg_fixture","type":"message","role":"assistant","model":"claude-upstream",
            "content":[{"type":"tool_use","id":"toolu_1","name":"editor__patch","input":{"input":"diff"}}],
            "stop_reason":"tool_use",
            "usage":{"input_tokens":4,"output_tokens":2,
                "cache_read_input_tokens":6,"cache_creation_input_tokens":3}
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = fixture("responses-anthropic-buffered", &upstream).await;
    let response = send_response_usage_request(
        &fixture,
        &json!({
            "model":fixture.model,
            "instructions":"Use the provided tool.",
            "input":[{"role":"user","content":[
                {"type":"input_text","text":"Apply this change"},
                {"type":"input_image","image_url":"data:image/png;base64,fixture"}
            ]}],
            "tools":[{"type":"namespace","name":"editor","tools":[
                {"type":"custom","name":"patch","description":"Apply a patch"}
            ]}],
            "tool_choice":{"type":"custom","namespace":"editor","name":"patch"},
            "stream":false,
            "max_output_tokens":64
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["status"], "completed");
    assert_eq!(body["model"], fixture.model);
    assert_eq!(body["output"][0]["type"], "custom_tool_call");
    assert_eq!(body["output"][0]["namespace"], "editor");
    assert_eq!(body["output"][0]["name"], "patch");
    assert_eq!(body["usage"]["input_tokens"], 13);

    let requests = upstream.received_requests().await.unwrap();
    let request = requests.first().expect("one Anthropic request");
    let forwarded: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(forwarded["model"], fixture.upstream_model);
    assert_eq!(forwarded["system"][0]["text"], "Use the provided tool.");
    assert_eq!(forwarded["messages"][0]["content"][1]["type"], "image");
    assert_eq!(forwarded["tools"][0]["name"], "editor__patch");
    assert_eq!(forwarded["tool_choice"]["name"], "editor__patch");
    upstream.verify().await;
}

#[tokio::test]
async fn responses_route_translates_anthropic_stream_and_drops_ping() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            include_str!("../../../../tests/fixtures/anthropic/claude_code_tool_stream.sse"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = fixture("responses-anthropic-stream", &upstream).await;
    let response = send_response_usage_request(
        &fixture,
        &json!({
            "model":fixture.model,
            "input":"Run pwd",
            "tools":[{"type":"function","name":"run","parameters":{
                "type":"object","properties":{"command":{"type":"string"}}
            }}],
            "stream":true,
            "max_output_tokens":64
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let wire = String::from_utf8(body.to_vec()).unwrap();
    assert!(!wire.contains("event: ping"));
    assert_eq!(wire.matches("event: response.completed\n").count(), 1);
    assert!(wire.contains("event: response.function_call_arguments.done"));
    assert!(wire.contains("\"input_tokens\":8"));
    assert!(wire.contains("\"output_tokens\":3"));
    upstream.verify().await;
}
