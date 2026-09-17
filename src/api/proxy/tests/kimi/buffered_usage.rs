use super::*;

#[tokio::test]
async fn native_kimi_buffered_chat_settles_discounted_cache_and_archives_original_body_once() {
    let upstream = MockServer::start().await;
    let fixture = response_usage_fixture("kimi-buffered-cache", &upstream, 0).await;
    let body = json!({"id":"kimi-native-buffered","object":"chat.completion","model":"kimi-k3",
        "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12,"cached_tokens":6}});
    let raw_body = serde_json::to_vec(&body).unwrap();
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(raw_body.clone(), "application/json"))
        .expect(1)
        .mount(&upstream)
        .await;
    let key = fixture
        .state
        .db
        .authenticate_key(&fixture.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let price = fixture
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
    let request_id = Uuid::now_v7();
    let reservation = fixture
        .state
        .db
        .start_proxy_request(StartProxyRequest {
            request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 20,
            output_token_ceiling: 8,
            protocol: "openai",
            model: &fixture.model,
            request_object: &format!("gap://{request_id}/request"),
            upstream_account_id: Some(fixture.upstream_account_id),
            model_route_id: Some(fixture.route_id),
        })
        .await
        .unwrap();
    let request = BufferedRequest {
        state: &fixture.state,
        reservation,
        request_id,
        started: Instant::now(),
        input_token_ceiling: 20,
        output_token_ceiling: 8,
        requested_service_tier: None,
        conversation: Some(ProxyConversation {
            key: key.clone(),
            request_body: Bytes::from(
                serde_json::to_vec(
                    &json!({"model":fixture.model,"messages":[{"role":"user","content":"test cached input"}]}),
                )
                .unwrap(),
            ),
            hints: crate::conversation::ConversationHints::default(),
            client_name: None,
            projection_admission: std::sync::Mutex::new(
                ConversationProjectionAdmission::Deferred,
            ),
        }),
        protocol: Protocol::OpenAiChat,
        tenant_id: key.tenant_id,
        memory: fixture.state.proxy_memory_budget.reservation(),
    };
    let mut attempt = UpstreamAttemptGuard::new(
        &fixture.state,
        request_id,
        fixture.route_id,
        fixture.upstream_account_id,
        1,
        UpstreamAttemptAdmission::Healthy {
            failure_epoch: Uuid::now_v7(),
        },
        None,
    );
    let raw = reqwest::Client::new()
        .post(upstream.uri())
        .send()
        .await
        .unwrap();
    // Exercise the production buffered finalizer and its real reservation,
    // accounting, and archive path without relaxing Kimi's fixed network origin.
    let response = finish_non_sse_proxy_response(NonSseProxyResponseInput {
        buffered_request: &request,
        selected_driver: crate::oauth::managed::kimi::PROVIDER_DRIVER,
        upstream: raw.into(),
        status: StatusCode::OK,
        content_type: Some(HeaderValue::from_static("application/json")),
        protocol: Protocol::OpenAiChat,
        capture_json_usage: true,
        upstream_attempt: &mut attempt,
    })
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap()
            .as_ref(),
        raw_body.as_slice()
    );
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            rows[0].input_tokens,
            rows[0].cached_input_tokens,
            rows[0].output_tokens
        ),
        (10, 6, 2),
        "request rows report inclusive input; cached input remains a subset"
    );
    assert_eq!(
        rows[0].usage_basis,
        Some(crate::model::RequestUsageBasis::ProviderReported)
    );
    // 4 uncached * 2 + 6 cached * 1 + 2 output * 3 = 20 microdollars.
    assert_eq!(
        rows[0].cost.parse::<Decimal>().unwrap(),
        Decimal::new(20, 6)
    );
    assert_exactly_once_side_effects(&fixture, request_id, None).await;
    drain_completed_response_archive(&fixture).await;
    let refs = fixture
        .state
        .db
        .request_archive_refs(fixture.key_id, request_id)
        .await
        .unwrap();
    let locator = refs.response_object.unwrap();
    assert!(!locator.starts_with("gap://"));
    assert_eq!(fixture.state.archive.get(&locator).await.unwrap(), raw_body);
    upstream.verify().await;
}
