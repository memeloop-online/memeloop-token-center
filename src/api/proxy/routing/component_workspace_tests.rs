use super::*;
use crate::gateway_body::memory::ProxyMemoryBudget;

#[tokio::test]
async fn component_materialization_retains_only_the_dispatched_prepared_body() {
    let (mut state, directory) = crate::api::tests::test_state().await;
    let root = directory.path().join("component-plugins");
    let package = root.join("example-provider");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("plugin.json"),
        include_bytes!("../../../../.plugin-examples/examples/plugins/policy-rewrite/plugin.json"),
    )
    .unwrap();
    std::fs::write(
        package.join("plugin.wasm"),
        include_bytes!("../../../../.plugin-examples/examples/plugins/policy-rewrite/plugin.wasm"),
    )
    .unwrap();
    state.plugins = crate::plugin::PluginRuntime::load(root.to_str(), state.db.clone()).unwrap();
    state.proxy_memory_budget = std::sync::Arc::new(ProxyMemoryBudget::new(256 * 1024));
    let blocker = state.proxy_memory_budget.reservation();
    assert!(blocker.try_grow(192 * 1024, 1));
    let request =
        json!({"model":"public-model","messages":[{"role":"user","content":"x".repeat(8192)}]});
    let source_length = crate::gateway_body::memory::json_encoded_length(&request).unwrap();
    let context = RequestContext {
        tenant_id: Uuid::now_v7().to_string(),
        principal_id: Uuid::now_v7().to_string(),
        key_id: Uuid::now_v7().to_string(),
        protocol: "openai".into(),
        model: "public-model".into(),
        config_json: "{}".into(),
    };
    let planned = PlannedProxyRoute {
        request_workspace: None,
        route: ResolvedUpstream {
            route_id: Uuid::now_v7(),
            account_id: Uuid::now_v7(),
            transport_revision: 1,
            credential_generation: 1,
            driver: "example-oauth-http".into(),
            base_url: "https://upstream.invalid".into(),
            config: json!({}),
            upstream_model: "vendor-model".into(),
            credential: crate::provider::UpstreamCredential::None,
        },
        forwarded_json: request,
        output_token_ceiling: 1,
        output_token_limit: Some(1),
        upstream_stream: false,
        codex_downstream_stream: false,
        codex_store_disabled: false,
        codex_session_id: None,
        component_context: Some(context),
        responses_chat: None,
        upstream_path: "/v1/chat/completions",
        wait_for_compaction: false,
        compact_v2_bridge: false,
        wrap_compact_as_sse: false,
        responses_anthropic: None,
        wire_shim_context: None,
    };
    let prepared = materialize_proxy_route(&state, planned).await.unwrap();
    assert!(
        prepared.forwarded_body.is_empty(),
        "component dispatch must not retain a second, unused serialization of its input"
    );
    let (component, returned_context) = prepared.component_request.as_ref().unwrap();
    assert_eq!(component.method, reqwest::Method::POST);
    assert_eq!(component.path, "/vendor/infer");
    assert_eq!(component.headers["x-plugin-shape"], "buffered-v1");
    assert_eq!(
        serde_json::from_slice::<Value>(&component.body).unwrap(),
        json!({"prompt":"from-component","model":"vendor-model"})
    );
    assert_eq!(returned_context.model, "public-model");
    assert!(
        prepared_input_reservation_bound(&prepared, source_length).unwrap() >= source_length as i64
    );
    drop(prepared);
    assert_eq!(state.proxy_memory_budget.snapshot().0, 192 * 1024);
    drop(blocker);
    assert_eq!(state.proxy_memory_budget.snapshot().0, 0);
}
