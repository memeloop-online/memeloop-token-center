use super::*;

#[tokio::test]
async fn translated_kimi_clean_eof_and_done_settle_and_archive_once() {
    for with_done in [false, true] {
        let upstream = MockServer::start().await;
        let bridge = MockServer::start().await;
        let fixture = response_usage_fixture("kimi-terminal", &bridge, 0).await;
        let mut wire = format!(
            "data: {}\n\n",
            json!({"id":"kimi-test","object":"chat.completion.chunk","model":"k3",
            "choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}})
        );
        if with_done {
            wire.push_str("data: [DONE]\n\n");
        }
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
            crate::api::kimi_transport::responses::Context::new(&json!({"model":fixture.model})),
            true,
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
        let ResponsesSseOutcome::Completed { response_id } = capture.finish_summary().outcome
        else {
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
        assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (5, 2));
        assert_eq!(
            rows[0].usage_basis,
            Some(crate::model::RequestUsageBasis::ProviderReported)
        );
        assert_eq!(rows[0].cost.parse::<Decimal>().unwrap(), Decimal::new(7, 6));
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
