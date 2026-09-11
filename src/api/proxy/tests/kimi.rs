use super::*;

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
