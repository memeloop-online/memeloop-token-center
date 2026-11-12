use super::*;

#[tokio::test]
async fn native_kimi_chat_uses_its_prepaid_workspace_under_budget_contention() {
    let (mut state, _directory) = crate::api::tests::test_state().await;
    state.proxy_memory_budget = std::sync::Arc::new(
        crate::gateway_body::memory::ProxyMemoryBudget::new(8 * 1024 * 1024),
    );
    let memory = state.proxy_memory_budget.reservation();
    let request = json!({
        "model":"k3.1-pro", "messages":[{"role":"user","content":"x".repeat(1024 * 1024)}],
        "stream":true, "stream_options":{"include_usage":true}, "max_tokens":1
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
        upstream_model: "k3.1-pro".into(),
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
        route,
        preparation_now: unix_millis(),
    })
    .unwrap();
    assert!(planned.request_workspace.is_some());
    assert_eq!(state.proxy_memory_budget.snapshot(), before);
    drop(request);
    let mut prepared = materialize_proxy_route(&state, planned).await.unwrap();
    assert_eq!(state.proxy_memory_budget.snapshot(), before);
    let forwarded: Value = serde_json::from_slice(&prepared.forwarded_body).unwrap();
    let content = forwarded["messages"][0]["content"].as_str().unwrap();
    assert_eq!(content.len(), 1024 * 1024);
    assert!(content.bytes().all(|byte| byte == b'x'));
    assert_eq!(forwarded["stream_options"]["include_usage"], true);
    assert_eq!(forwarded["max_tokens"], 1);
    assert!(memory.borrow_route_workspace(1).is_none());
    prepared.release_request_buffers();
    assert!(prepared.forwarded_body.is_empty());
    assert!(memory.borrow_route_workspace(1).is_some());
    drop(prepared);
    drop(memory);
    drop(blocker);
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
    ] {
        assert!(!kimi::reuses_chat_workspace(
            Protocol::OpenAiChat,
            &json!({"messages":[message]})
        ));
    }
}
