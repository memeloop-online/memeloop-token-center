use super::*;
use crate::{
    config::Config,
    db::{CreateKeyInput, CreateModelRouteInput, CreateUpstreamAccountInput},
    gateway_body::memory::ProxyMemoryBudget,
    model::KeyPolicy,
    plugin::PluginRuntime,
    provider::UpstreamCredential,
};
use rust_decimal::Decimal;
use serde_json::json;
use std::sync::Arc;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::layer::SubscriberExt;
use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
use wit_parser::Resolve;

async fn fixture(post_auth: Option<&str>) -> (tempfile::TempDir, AppState, AuthenticatedKey) {
    fixture_with_normalize_log(post_auth, false).await
}

async fn fixture_with_normalize_log(
    post_auth: Option<&str>,
    normalize_log: bool,
) -> (tempfile::TempDir, AppState, AuthenticatedKey) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("plugins");
    let package = root.join("example-policy-rewrite");
    std::fs::create_dir_all(&package).unwrap();
    let mut manifest: Value = serde_json::from_str(include_str!(
        "../../../.plugin-examples/examples/plugins/policy-rewrite/plugin.json"
    ))
    .unwrap();
    if normalize_log {
        manifest["capabilities"] = json!([{"kind":"log"}]);
    }
    std::fs::write(
        package.join("plugin.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let mut source =
        include_str!("../../../.plugin-examples/examples/plugins/policy-rewrite/plugin.wat")
            .replace("(memory $memory 1)", "(memory $memory 512)");
    if let Some(body) = post_auth {
        let (prefix, remaining) = source.split_once(";; BEGIN POST-AUTH BODY").unwrap();
        let (_, suffix) = remaining.split_once(";; END POST-AUTH BODY").unwrap();
        source = format!("{prefix}{body}{suffix}");
    }
    if normalize_log {
        source = source.replace(
            ";; BEGIN NORMALIZE BODY",
            ";; BEGIN NORMALIZE BODY
             i32.const 0 i32.const 0 i32.const 0 i32.const 0 call $log",
        );
    }
    let mut module = wat::parse_str(source).unwrap();
    let mut resolve = Resolve::default();
    let (package_id, _) = resolve.push_path("wit/token-center.wit").unwrap();
    let world = resolve.select_world(&[package_id], Some("plugin")).unwrap();
    embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8).unwrap();
    let component = ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap();
    std::fs::write(package.join("plugin.wasm"), component).unwrap();
    let config = Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("plugin-memory.db").display()
    ));
    let mut state = AppState::initialize(config).await.unwrap();
    state.plugins = PluginRuntime::load(root.to_str(), state.db.clone()).unwrap();
    state.proxy_memory_budget = Arc::new(ProxyMemoryBudget::new(1024 * 1024));
    let issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "plugin-memory".into(),
                principal_external_id: "member".into(),
                alias: "plugin-memory".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ONE,
                idempotency_key: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let key = state
        .db
        .authenticate_key(&issued.key, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    (directory, state, key)
}

fn policy_body(allow: bool, rewrite: bool) -> String {
    format!(
        "i32.const 256 i32.const 0 i32.store
         i32.const 260 i32.const {} i32.store
         i32.const 264 i32.const 0 i32.store
         i32.const 276 i32.const 0 i32.store
         i32.const 288 i32.const 0 i32.store
         i32.const 300 i32.const {} i32.store
         i32.const 304 local.get 12 i32.store
         i32.const 308 local.get 13 i32.store
         i32.const 256",
        u8::from(allow),
        u8::from(rewrite)
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn occupied_budget_keeps_large_real_policy_allow_deny_and_rewrite_semantics() {
    for (allow, rewrite) in [(true, false), (false, true), (true, true)] {
        let (_directory, state, key) = fixture(Some(&policy_body(allow, rewrite))).await;
        let held = state.proxy_memory_budget.reservation();
        assert!(held.try_grow(1024 * 1024, 1));
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..2 {
            let state = state.clone();
            let key = key.clone();
            tasks.spawn(async move {
                let memory = state.proxy_memory_budget.reservation();
                let owner = Arc::downgrade(&memory);
                let content = "x".repeat(24 * 1024 * 1024);
                let result = apply_traffic_plugin(
                    &state,
                    &key,
                    "openai",
                    json!({"model":"requested-model", "messages":[{"role":"user", "content":content}]}),
                    "requested-model".into(),
                    Some(memory),
                )
                .await;
                assert!(owner.upgrade().is_none(), "completed hook releases its owner");
                if allow {
                    let applied = result.unwrap();
                    assert_eq!(applied.model, "requested-model");
                    assert_eq!(applied.request_rewrite_supplied, rewrite);
                    let content = applied.request_json["messages"][0]["content"]
                        .as_str()
                        .unwrap();
                    assert_eq!(content.len(), 24 * 1024 * 1024);
                    assert!(content.bytes().all(|byte| byte == b'x'));
                } else {
                    assert!(matches!(result, Err(AppError::Forbidden)));
                }
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
        assert_eq!(state.proxy_memory_budget.snapshot().0, 1024 * 1024);
        drop(held);
        assert_eq!(state.proxy_memory_budget.snapshot().0, 0);
    }
}

#[tokio::test]
async fn occupied_budget_still_prepares_and_normalizes_the_component_protocol() {
    let (_directory, state, key) = fixture(None).await;
    let held = state.proxy_memory_budget.reservation();
    assert!(held.try_grow(1024 * 1024, 1));
    let context = RequestContext {
        tenant_id: key.tenant_id.to_string(),
        principal_id: key.principal_id.to_string(),
        key_id: key.key_id.to_string(),
        protocol: "openai".into(),
        model: "example-rewritten".into(),
        config_json: "{}".into(),
    };
    let memory = state.proxy_memory_budget.reservation();
    let owner = Arc::downgrade(&memory);
    let prepared = prepare_component_provider(
        &state,
        "example-oauth-http",
        context.clone(),
        json!({"base_url":"https://example.com"}),
        json!({"model":"example-rewritten", "messages":[]}),
        memory,
    )
    .await
    .unwrap();
    assert!(owner.upgrade().is_none());
    assert_eq!(prepared.method, reqwest::Method::POST);
    assert_eq!(prepared.path, "/vendor/infer");
    assert_eq!(prepared.headers["x-plugin-shape"], "buffered-v1");
    assert_eq!(
        serde_json::from_slice::<Value>(&prepared.body).unwrap(),
        json!({"prompt":"from-component", "model":"vendor-model"})
    );
    let memory = state.proxy_memory_budget.reservation();
    let owner = Arc::downgrade(&memory);
    let normalized = normalize_component_provider(
        &state,
        "example-oauth-http",
        context,
        207,
        BTreeMap::from([("content-type".into(), "application/vnd.vendor+json".into())]),
        vec![b'x'; 1024 * 1024],
        memory,
    )
    .await
    .unwrap();
    assert!(owner.upgrade().is_none());
    assert_eq!(normalized.status, 200);
    assert_eq!(normalized.headers["content-type"], "application/json");
    assert_eq!(normalized.input_tokens, 7);
    assert_eq!(normalized.output_tokens, 3);
    assert!(!normalized.estimated);
    assert_eq!(
        serde_json::from_slice::<Value>(&normalized.body).unwrap(),
        json!({
            "id":"cmp-1", "object":"chat.completion",
            "choices":[{"message":{"role":"assistant","content":"normalized by component"}}],
            "usage":{"prompt_tokens":7,"completion_tokens":3}
        })
    );
    assert_eq!(state.proxy_memory_budget.snapshot().0, 1024 * 1024);
    drop(held);
    assert_eq!(state.proxy_memory_budget.snapshot().0, 0);
}

struct NormalizeLogGate {
    entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    finished: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for NormalizeLogGate {
    fn drop(&mut self) {
        if let Some(finished) = self.finished.take() {
            let _ = finished.send(());
        }
    }
}

impl<Subscriber: tracing::Subscriber> tracing_subscriber::Layer<Subscriber> for NormalizeLogGate {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, Subscriber>,
    ) {
        if event.metadata().target() == "memeloop_token_center::plugin"
            && event.metadata().fields().field("decision_code").is_some()
            && let Some(entered) = self.entered.lock().unwrap().take()
        {
            entered.send(()).unwrap();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .expect("test releases the real normalize guest hostcall");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_normalize_keeps_existing_memory_owner_until_guest_exit_after_caller_abort() {
    let (_directory, state, key) = fixture_with_normalize_log(None, true).await;
    for cancel in [false, true] {
        let memory = state.proxy_memory_budget.reservation();
        assert!(memory.try_grow(64 * 1024, 1));
        assert!(memory.reserve_buffered_response(64 * 1024, tokio::time::Instant::now()));
        let owner = Arc::downgrade(&memory);
        let admitted = state.proxy_memory_budget.snapshot().0;
        let context = RequestContext {
            tenant_id: key.tenant_id.to_string(),
            principal_id: key.principal_id.to_string(),
            key_id: key.key_id.to_string(),
            protocol: "openai".into(),
            model: "example-rewritten".into(),
            config_json: "{}".into(),
        };
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let (finished, mut exited) = tokio::sync::oneshot::channel();
        let subscriber = tracing_subscriber::registry().with(NormalizeLogGate {
            entered: std::sync::Mutex::new(Some(entered)),
            release: std::sync::Mutex::new(blocked),
            finished: Some(finished),
        });
        let invocation_state = state.clone();
        let mut caller = Some(tokio::spawn(
            async move {
                normalize_component_provider(
                    &invocation_state,
                    "example-oauth-http",
                    context,
                    200,
                    BTreeMap::new(),
                    vec![b'x'; 64 * 1024],
                    memory,
                )
                .await
            }
            .with_subscriber(subscriber),
        ));
        tokio::time::timeout(Duration::from_secs(5), started)
            .await
            .expect("real normalize guest enters its log hostcall")
            .unwrap();
        if cancel {
            let caller = caller.take().unwrap();
            caller.abort();
            assert!(caller.await.unwrap_err().is_cancelled());
        }
        assert!(owner.upgrade().is_some());
        assert_eq!(state.proxy_memory_budget.snapshot().0, admitted);
        let replacement = state.proxy_memory_budget.reservation();
        assert!(!replacement.try_grow(1024 * 1024, 1));
        assert!(matches!(
            exited.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        release.send(()).unwrap();
        if let Some(caller) = caller {
            assert_eq!(caller.await.unwrap().unwrap().status, 200);
        }
        tokio::time::timeout(Duration::from_secs(5), exited)
            .await
            .expect("normalize blocking invocation tears down its dispatch after work exits")
            .unwrap();
        assert!(owner.upgrade().is_none());
        assert_eq!(state.proxy_memory_budget.snapshot().0, 0);
        assert!(replacement.try_grow(1024 * 1024, 1));
        drop(replacement);
        assert_eq!(state.proxy_memory_budget.snapshot().0, 0);
    }
}

#[tokio::test]
async fn occupied_budget_never_grants_requested_rewritten_or_cross_tenant_models() {
    let (_directory, state, _key) = fixture(None).await;
    let held = state.proxy_memory_budget.reservation();
    assert!(held.try_grow(1024 * 1024, 1));
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "plugin-memory".into(),
                name: "plugin-memory".into(),
                driver: "http-json".into(),
                config: json!({"base_url":"https://example.com"}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let mut routes = Vec::new();
    for model in ["requested-model", "example-rewritten"] {
        routes.push(
            state
                .db
                .create_model_route(CreateModelRouteInput {
                    tenant_external_id: "plugin-memory".into(),
                    public_model: model.into(),
                    upstream_account_id: account.id,
                    upstream_model: "vendor-model".into(),
                    protocol: "openai".into(),
                    priority: 0,
                })
                .await
                .unwrap()
                .id,
        );
    }
    for grant_count in 0..=2 {
        let issued = state
            .db
            .create_key_with_routing(
                CreateKeyInput {
                    tenant_external_id: "plugin-memory".into(),
                    principal_external_id: "member".into(),
                    alias: format!("grants-{grant_count}"),
                    currency: "USD".into(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::ONE,
                    idempotency_key: None,
                },
                &routes[..grant_count],
                &[],
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        let mut authorized = state
            .db
            .authenticate_key(&issued.key, state.config.key_pepper.as_bytes())
            .await
            .unwrap();
        for cross_tenant in [false, true] {
            if cross_tenant {
                authorized.tenant_id = Uuid::new_v4();
            }
            let result = apply_traffic_policy_with_memory(
                &state,
                &authorized,
                TrafficPolicyProtocols::same("openai"),
                json!({"model":"requested-model","messages":[]}),
                state.proxy_memory_budget.reservation(),
            )
            .await;
            if grant_count == 2 && !cross_tenant {
                assert_eq!(result.unwrap().model, "example-rewritten");
            } else {
                assert!(matches!(result, Err(AppError::Forbidden)));
            }
        }
    }
    drop(held);
    assert_eq!(state.proxy_memory_budget.snapshot().0, 0);
}
