use super::*;

#[tokio::test]
async fn native_kimi_buffered_chat_settles_discounted_cache_and_archives_original_body_once() {
    native_kimi_buffered_usage(2).await;
}

#[tokio::test]
async fn native_kimi_output_above_requested_limit_preserves_success_and_actual_usage() {
    native_kimi_buffered_usage(19).await;
}

async fn native_kimi_buffered_usage(output_tokens: i64) {
    native_kimi_buffered_usage_with_route(output_tokens, None).await;
}

#[tokio::test]
async fn authenticated_buffered_response_scope_reaches_archive_selector_and_sql_binding() {
    for case in [(true, true), (false, true), (true, false)] {
        native_kimi_buffered_usage_with_route(2, Some(case)).await;
    }
}

async fn native_kimi_buffered_usage_with_route(output_tokens: i64, route: Option<(bool, bool)>) {
    let upstream = MockServer::start().await;
    let mut fixture = response_usage_fixture("kimi-buffered-cache", &upstream, 0).await;
    let body = json!({"id":"kimi-native-buffered","object":"chat.completion","model":"kimi-k3",
        "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":10,"completion_tokens":output_tokens,"total_tokens":10 + output_tokens,"cached_tokens":6}});
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
    if let Some((matches, _)) = route {
        fixture.state.archive = crate::archive::ArchiveStore::training_memory_for_test();
        let rule_key = if matches { key.key_id } else { Uuid::now_v7() };
        let rules = json!({"version":1,"rules":[{"tenant":key.tenant_id,"key":rule_key,"purpose":"response","backend_identity":"a".repeat(64)}]}).to_string();
        fixture
            .state
            .archive
            .set_response_write_rules_for_test(Some(&rules))
            .unwrap();
        // Match AppState initialization: all database readers share the same
        // backend registry as the writer, including qualified test locators.
        fixture.state.db = fixture
            .state
            .db
            .clone()
            .with_archive(fixture.state.archive.clone());
        fixture.state.persistence_db = fixture
            .state
            .persistence_db
            .clone()
            .with_archive(fixture.state.archive.clone());
        fixture.state.routing_persistence_db = fixture
            .state
            .routing_persistence_db
            .clone()
            .with_archive(fixture.state.archive.clone());
    }
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
            output_token_ceiling: 16,
            protocol: "openai",
            model: &fixture.model,
            request_object: &format!("gap://{request_id}/request"),
            upstream_account_id: Some(fixture.upstream_account_id),
            model_route_id: Some(fixture.route_id),
        })
        .await
        .unwrap();
    let request_body = Bytes::from(
        serde_json::to_vec(
            &json!({"model":fixture.model,"messages":[{"role":"user","content":"test cached input"}]}),
        )
        .unwrap(),
    );
    let raw_request = fixture
        .state
        .archive
        .put_tenant_content(key.tenant_id, request_body.clone())
        .await
        .unwrap();
    fixture
        .state
        .archive
        .publish_raw_capture(key.tenant_id, request_id, "request", &raw_request)
        .await
        .unwrap();
    let mut request = BufferedRequest {
        archive_write_scope: route
            .filter(|(_, present)| *present)
            .map(|_| crate::archive::TrustedArchiveWriteScope::from(&key)),
        session_preference: None,
        state: &fixture.state,
        reservation,
        request_id,
        memory_admission_wait: Duration::from_secs(30),
        input_token_ceiling: 20,
        output_token_ceiling: 16,
        output_token_limit: Some(16),
        requested_service_tier: None,
        conversation: Some(ProxyConversation {
            key: key.clone(),
            request_body: ConversationBody::InMemory(request_body),
            hints: crate::conversation::ConversationHints::default(),
            client_name: None,
            projection_admission: std::sync::Mutex::new(ConversationProjectionAdmission::Deferred),
        }),
        protocol: Protocol::OpenAiChat,
        tenant_id: key.tenant_id,
        original_response_capture: false,
        memory: fixture.state.proxy_memory_budget.reservation(),
    };
    let mut attempt = UpstreamAttemptGuard::new(
        &fixture.state,
        request_id,
        fixture.route_id,
        fixture.upstream_account_id,
        1,
        0,
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
    let captured = request.capture_original_response(raw.into()).await;
    // Exercise the production buffered finalizer and its real reservation,
    // accounting, and archive path without relaxing Kimi's fixed network origin.
    let response = finish_non_sse_proxy_response(NonSseProxyResponseInput {
        buffered_request: &request,
        selected_driver: crate::oauth::managed::kimi::PROVIDER_DRIVER,
        upstream: captured,
        status: StatusCode::OK,
        content_type: Some(HeaderValue::from_static("application/json")),
        protocol: Protocol::OpenAiChat,
        capture_json_usage: true,
        response_headers: HeaderMap::new(),
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
    wait_for_request_settlement(&fixture, 1).await;
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
        (10, 6, output_tokens),
        "request rows report inclusive input; cached input remains a subset"
    );
    assert_eq!(
        rows[0].usage_basis,
        Some(crate::model::RequestUsageBasis::ProviderReported)
    );
    // Preserve the actual output alongside the discounted cache price.
    assert_eq!(
        rows[0].cost.parse::<Decimal>().unwrap(),
        Decimal::new(4 * 2 + 6 + output_tokens * 3, 6)
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
    if let Some((matches, present)) = route {
        assert_eq!(locator.starts_with("stores/training/"), matches && present);
        // Rule removal cannot change the already published typed locator.
        fixture
            .state
            .archive
            .set_response_write_rules_for_test(None)
            .unwrap();
    }
    assert_eq!(fixture.state.archive.get(&locator).await.unwrap(), raw_body);
    upstream.verify().await;
}
