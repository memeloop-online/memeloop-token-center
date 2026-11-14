use super::*;

#[tokio::test]
async fn native_kimi_chat_uses_its_prepaid_workspace_under_budget_contention() {
    let (mut state, directory) = crate::api::tests::test_state().await;
    state.proxy_memory_budget = std::sync::Arc::new(
        crate::gateway_body::memory::ProxyMemoryBudget::new(8 * 1024 * 1024),
    );
    let memory = state.proxy_memory_budget.reservation();
    let mut request = json!({
        "model":"k3.1-pro", "messages":[{"role":"user","content":"x".repeat(1024 * 1024)}],
        "stream":true, "max_tokens":1
    });
    assert!(memory.try_reserve_json(&serde_json::to_vec(&request).unwrap()));
    // Model a competing request consuming the rest of the shared allowance.
    // There is no capacity for a duplicate planning/serialization reservation.
    let blocker = state.proxy_memory_budget.reservation();
    let (used, capacity, _, _) = state.proxy_memory_budget.snapshot();
    assert!(blocker.try_grow(capacity - used, 1));
    let before = state.proxy_memory_budget.snapshot();
    assert!(state.proxy_memory_budget.temporary(64 * 1024).is_err());
    let key = AuthenticatedKey {
        key_id: Uuid::now_v7(),
        tenant_id: Uuid::now_v7(),
        principal_id: Uuid::now_v7(),
        account_id: Uuid::now_v7(),
        alias: "kimi-workspace".into(),
        currency: "USD".into(),
        credential_generation: 1,
        policy: Default::default(),
    };
    let route = ResolvedUpstream {
        route_id: Uuid::now_v7(),
        account_id: Uuid::now_v7(),
        transport_revision: 1,
        credential_generation: 1,
        driver: crate::oauth::managed::kimi::PROVIDER_DRIVER.into(),
        base_url: crate::oauth::managed::kimi::BASE_URL.into(),
        config: json!({}),
        upstream_model: "k3.1-pro(long \"escaped model\")".into(),
        credential: crate::provider::UpstreamCredential::OAuth {
            access_token: "fixture-access".into(),
            refresh_token: Some("fixture-refresh".into()),
            expires_at: None,
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: Some(
                json!({"schema":"kimi-oauth-v1","device_id":"fixture-device","scope":"coding","token_type":"Bearer"}),
            ),
            proxy_url: None,
            proxy_network_scope: None,
        },
    };
    let candidates = [
        crate::provider::AuthorizedUpstreamCandidate {
            route_id: Uuid::now_v7(),
            account_id: Uuid::now_v7(),
            driver: "http-json".into(),
            upstream_model: "relay-primary".into(),
            transport_revision: 1,
            credential_generation: 1,
        },
        crate::provider::AuthorizedUpstreamCandidate {
            route_id: route.route_id,
            account_id: route.account_id,
            driver: route.driver.clone(),
            upstream_model: "k3.1-pro".into(),
            transport_revision: 1,
            credential_generation: 1,
        },
        crate::provider::AuthorizedUpstreamCandidate {
            route_id: Uuid::now_v7(),
            account_id: route.account_id,
            driver: route.driver.clone(),
            upstream_model: route.upstream_model.clone(),
            transport_revision: 1,
            credential_generation: 1,
        },
    ];
    let shape = native_chat_candidate_shape(Protocol::OpenAiChat, &mut request, &candidates)
        .unwrap()
        .unwrap();
    assert_eq!(request["model"], "k3.1-pro");
    assert!(request.get("stream_options").is_none());
    assert_eq!(state.proxy_memory_budget.snapshot(), before);
    let replay = super::super::request_source::spool_normalized(
        request,
        directory.path().to_owned(),
        &memory,
    )
    .await
    .unwrap();
    memory.release_unretained_json();
    let mut admission = Box::pin(super::super::request_source::parse_with_shape(
        replay.clone(),
        memory.clone(),
        shape.0,
        shape.1,
    ));
    assert!(matches!(
        futures_util::poll!(&mut admission),
        std::task::Poll::Pending
    ));
    // The competing request can release its owner while this request retains
    // only a complete local replay. No partial JSON charge blocks its exit.
    assert_eq!(state.proxy_memory_budget.snapshot().0, capacity - used);
    drop(admission);
    assert_eq!(memory.request_json_reserved_bytes(), 0);
    assert!(memory.borrow_route_workspace(1).is_none());
    assert_eq!(state.proxy_memory_budget.snapshot().0, capacity - used);
    assert!(shape.0 > replay.len());
    assert!(shape.1 > replay.json_nodes());
    let admission =
        super::super::request_source::parse_with_shape(replay, memory.clone(), shape.0, shape.1);
    drop(blocker);
    let request = admission.await.unwrap();
    assert_eq!(request["model"], "k3.1-pro");
    assert!(request.get("stream_options").is_none());
    for model in ["k3.1-pro", "k3.1-pro(long \"escaped model\")"] {
        let mut next_route = route.clone();
        next_route.upstream_model = model.into();
        let planned = plan_proxy_route(ProxyRoutePlanInput {
            request: ProxyRequestContext {
                state: &state,
                key: &key,
                model: "k3.1-pro",
                protocol: Protocol::OpenAiChat,
                request_id: Uuid::now_v7(),
                request_json: &request,
                request_memory: Some(&memory),
                codex_multi_agent_v2_request: false,
            },
            route: next_route,
            preparation_now: unix_millis(),
        })
        .unwrap();
        assert!(planned.request_workspace.is_some());
        let readmitted = state.proxy_memory_budget.snapshot();
        let mut prepared = materialize_proxy_route(&state, planned).await.unwrap();
        assert_eq!(state.proxy_memory_budget.snapshot(), readmitted);
        let forwarded: Value = serde_json::from_slice(&prepared.forwarded_body).unwrap();
        let content = forwarded["messages"][0]["content"].as_str().unwrap();
        assert_eq!(content.len(), 1024 * 1024);
        assert!(content.bytes().all(|byte| byte == b'x'));
        assert_eq!(forwarded["stream_options"]["include_usage"], true);
        assert_eq!(forwarded["max_tokens"], 1);
        assert_eq!(forwarded["model"], model);
        assert!(memory.borrow_route_workspace(1).is_none());
        prepared.release_request_buffers();
        assert!(prepared.forwarded_body.is_empty());
        assert!(memory.borrow_route_workspace(1).is_some());
        drop(prepared);
    }
    drop(request);
    drop(memory);
    assert_eq!(state.proxy_memory_budget.snapshot().0, 0);
}

#[test]
fn native_chat_histories_with_copied_tool_or_reasoning_state_keep_adapter_accounting() {
    let ordinary = json!({"messages":[{"role":"assistant","content":"prior answer"},{"role":"user","content":"next"}]});
    assert!(kimi::reuses_chat_workspace(Protocol::OpenAiChat, &ordinary));
    assert!(!kimi::reuses_chat_workspace(
        Protocol::OpenAiResponses,
        &ordinary
    ));
    for message in [
        json!({"role":"assistant","tool_calls":[{"id":"a","function":{"arguments":"large arguments"}}]}),
        json!({"role":"assistant","reasoning_content":"retained reasoning"}),
        json!({"role":" assistant ","tool_calls":[{"id":"a"}]}),
        json!({"role":" assistant ","reasoning_content":"retained reasoning"}),
        json!({"role":"tool","tool_call_id":"large explicit identifier","content":"result"}),
        json!({"role":" tool ","call_id":"large fallback identifier","content":"result"}),
    ] {
        assert!(!kimi::reuses_chat_workspace(
            Protocol::OpenAiChat,
            &json!({"messages":[message]})
        ));
    }
}

#[test]
fn native_chat_metadata_shape_covers_all_candidates_and_restores_source() {
    let candidates = ["k2.7-code", "k3.1-pro(long \"escaped model\")"].map(|model| {
        crate::provider::AuthorizedUpstreamCandidate {
            route_id: Uuid::now_v7(),
            account_id: Uuid::now_v7(),
            driver: crate::oauth::managed::kimi::PROVIDER_DRIVER.into(),
            upstream_model: model.into(),
            transport_revision: 1,
            credential_generation: 1,
        }
    });
    for options in [
        None,
        Some(json!({})),
        Some(json!({"include_usage":false,"extension":{"text":"保留"}})),
        Some(json!({"include_usage":"replaced client value"})),
    ] {
        let mut request = json!({"model":"public alias", "stream":true,
            "messages":[{"role":"user","content":"正文"}]});
        if let Some(options) = options {
            request["stream_options"] = options;
        }
        let original = request.clone();
        let shape = native_chat_candidate_shape(Protocol::OpenAiChat, &mut request, &candidates)
            .unwrap()
            .unwrap();
        assert_eq!(request, original);
        for candidate in &candidates {
            let mut actual = request.clone();
            crate::api::kimi_transport::prepare(
                Protocol::OpenAiChat,
                &candidate.upstream_model,
                &mut actual,
            )
            .unwrap();
            let actual_shape = crate::gateway_body::memory::json_encoded_shape(&actual).unwrap();
            assert!(actual_shape.0 <= shape.0);
            assert!(actual_shape.1 <= shape.1);
        }
    }
    let mut malformed = json!({"model":"public alias","stream":true,
        "stream_options":[],"messages":[{"role":"user","content":"body"}]});
    let original = malformed.clone();
    assert!(
        native_chat_candidate_shape(Protocol::OpenAiChat, &mut malformed, &candidates)
            .unwrap()
            .is_none()
    );
    assert_eq!(malformed, original);
}
