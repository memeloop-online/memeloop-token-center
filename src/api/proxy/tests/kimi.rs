use super::*;

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
    let context = crate::api::kimi_transport::responses::Context::new(&json!({"model":"kimi"}));
    let translated = routing::kimi::translate(response, context, false).unwrap();
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
