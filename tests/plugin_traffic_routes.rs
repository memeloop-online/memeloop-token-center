use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{
        CreateKeyInput, CreateModelRouteInput, CreateUpstreamAccountInput, StatsFilter, unix_millis,
    },
    model::{IssuedKey, KeyPolicy},
    provider::UpstreamCredential,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tower::ServiceExt;
use tracing::instrument::WithSubscriber;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, header as matches_header, method, path},
};
use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
use wit_parser::Resolve;

#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

struct LogWriter(LogCapture);

impl std::io::Write for LogWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.0.lock().unwrap().extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for LogCapture {
    type Writer = LogWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        LogWriter(self.clone())
    }
}

impl LogCapture {
    fn contents(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

fn component_from_core_wat(source: &str) -> Vec<u8> {
    let mut module = wat::parse_str(source).expect("parse test core Wasm");
    let mut resolve = Resolve::default();
    let (package, _) = resolve
        .push_path("wit/token-center.wit")
        .expect("parse plugin WIT");
    let world = resolve
        .select_world(&[package], Some("plugin"))
        .expect("select plugin world");
    embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8)
        .expect("embed plugin metadata");
    ComponentEncoder::default()
        .module(&module)
        .expect("read test core module")
        .validate(true)
        .encode()
        .expect("encode test component")
}

fn policy_wat(body: &str) -> String {
    let source = include_str!("../examples/plugins/policy-rewrite/plugin.wat");
    let (prefix, tail) = source
        .split_once(";; BEGIN POST-AUTH BODY")
        .expect("post-auth start marker");
    let (_, suffix) = tail
        .split_once(";; END POST-AUTH BODY")
        .expect("post-auth end marker");
    format!("{prefix};; BEGIN POST-AUTH BODY\n{body}\n;; END POST-AUTH BODY{suffix}")
}

fn write_policy_package(root: &Path, body: &str, capabilities: Value) {
    fs::write(
        root.join("plugin.json"),
        serde_json::to_vec(&json!({
            "id": "gateway-policy",
            "version": "1.0.0",
            "wit_version": "0.2.0",
            "wasm": "plugin.wasm",
            "capabilities": capabilities,
            "contributions": {"traffic_policy": true, "providers": []}
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        root.join("plugin.wasm"),
        component_from_core_wat(&policy_wat(body)),
    )
    .unwrap();
}

async fn policy_state(
    body: &str,
    label: &str,
    allowed_models: Vec<String>,
) -> (tempfile::TempDir, AppState, IssuedKey) {
    policy_state_with_capabilities(body, label, allowed_models, json!([])).await
}

async fn policy_state_with_capabilities(
    body: &str,
    label: &str,
    allowed_models: Vec<String>,
    capabilities: Value,
) -> (tempfile::TempDir, AppState, IssuedKey) {
    let directory = tempfile::tempdir().unwrap();
    write_policy_package(directory.path(), body, capabilities);
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("policy.db").display()
    );
    let mut config = Config::for_test(database_url);
    config.plugin_dir = Some(directory.path().display().to_string());
    let state = AppState::initialize(config).await.unwrap();
    let issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: format!("policy-{label}"),
                principal_external_id: "policy-user".into(),
                alias: "policy-key".into(),
                currency: "USD".into(),
                policy: KeyPolicy {
                    allowed_models,
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    (directory, state, issued)
}

const DENY_BODY: &str = r#"
    i32.const 256 i32.const 0 i32.store
    i32.const 260 i32.const 0 i32.store
    i32.const 264 i32.const 0 i32.store
    i32.const 276 i32.const 0 i32.store
    i32.const 288 i32.const 0 i32.store
    i32.const 300 i32.const 0 i32.store
    i32.const 256
"#;

const REWRITE_MODEL_BODY: &str = r#"
    i32.const 256 i32.const 0 i32.store
    i32.const 260 i32.const 1 i32.store
    i32.const 264 i32.const 0 i32.store
    i32.const 276 i32.const 1 i32.store
    i32.const 280 i32.const 400 i32.store
    i32.const 284 i32.const 17 i32.store
    i32.const 288 i32.const 0 i32.store
    i32.const 300 i32.const 0 i32.store
    i32.const 256
"#;

fn deny_body_with_reason(reason: &str) -> String {
    let stores = stores_for_string(4096, reason);
    format!(
        r#"
        {stores}
        i32.const 256 i32.const 0 i32.store
        i32.const 260 i32.const 0 i32.store
        i32.const 264 i32.const 1 i32.store
        i32.const 268 i32.const 4096 i32.store
        i32.const 272 i32.const {reason_length} i32.store
        i32.const 276 i32.const 0 i32.store
        i32.const 288 i32.const 0 i32.store
        i32.const 300 i32.const 0 i32.store
        i32.const 256
        "#,
        reason_length = reason.len()
    )
}

fn stores_for_string(pointer: usize, value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .enumerate()
        .map(|(index, byte)| format!("i32.const {} i32.const {byte} i32.store8", pointer + index))
        .collect::<Vec<_>>()
        .join("\n")
}

fn deny_body_with_log_message(message: &str) -> String {
    let level_stores = stores_for_string(4080, "warn");
    let message_stores = stores_for_string(4096, message);
    format!(
        r#"
        {level_stores}
        {message_stores}
        i32.const 4080 i32.const 4
        i32.const 4096 i32.const {message_length}
        call $log
        i32.const 256 i32.const 0 i32.store
        i32.const 260 i32.const 0 i32.store
        i32.const 264 i32.const 0 i32.store
        i32.const 276 i32.const 0 i32.store
        i32.const 288 i32.const 0 i32.store
        i32.const 300 i32.const 0 i32.store
        i32.const 256
        "#,
        message_length = message.len()
    )
}

#[tokio::test]
async fn deny_policy_covers_text_images_seedance_and_comfyui_before_any_upstream_or_charge() {
    let (_directory, state, issued) = policy_state(
        DENY_BODY,
        "deny",
        vec!["requested-model".into(), "example-rewritten".into()],
    )
    .await;
    for (path, body) in [
        (
            "/v1/chat/completions",
            json!({"model": "requested-model", "messages": []}),
        ),
        (
            "/v1/images/generations",
            json!({"model": "requested-model", "prompt": "blocked image"}),
        ),
        (
            "/v1/videos/generations",
            json!({"model": "requested-model", "input": {"duration": 5}}),
        ),
        (
            "/v1/generations",
            json!({"model": "requested-model", "input": {"parameters": {}}}),
        ),
    ] {
        let response = call(&state, &issued.key, path, body).await;
        assert_eq!(response.0, StatusCode::FORBIDDEN, "{path}");
    }
    let key = state
        .db
        .authenticate_key(&issued.key, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    assert_eq!(
        state
            .db
            .stats_filtered(
                key.key_id,
                StatsFilter {
                    from_created_at: Some(unix_millis().saturating_sub(60_000)),
                    to_created_at: Some(unix_millis().saturating_add(1)),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .summary
            .total_requests,
        0
    );
}

#[tokio::test]
async fn malicious_denial_reason_is_absent_from_logs_and_http_response() {
    const CANARY: &str = "CANARY_TRAFFIC_REASON_CUSTOMER_SECRET";
    let reason = format!("{CANARY}{}", "\0\u{1b}\n".repeat(1_024));
    let body = deny_body_with_reason(&reason);
    let (_directory, state, issued) =
        policy_state(&body, "malicious-reason", vec!["requested-model".into()]).await;
    let capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(capture.clone())
        .finish();

    let response = call(
        &state,
        &issued.key,
        "/v1/chat/completions",
        json!({"model": "requested-model", "messages": []}),
    )
    .with_subscriber(subscriber)
    .await;

    assert_eq!(response.0, StatusCode::FORBIDDEN);
    let response_body = serde_json::to_string(&response.1).unwrap();
    let logs = capture.contents();
    assert!(!response_body.contains(CANARY), "{response_body}");
    assert!(!logs.contains(CANARY), "{logs}");
    assert!(logs.contains("gateway-policy"), "{logs}");
    assert!(logs.contains("policy_denied_invalid_metadata"), "{logs}");
    assert!(logs.len() < 4_096, "guest reason amplified log output");
}

#[tokio::test]
async fn log_capability_emits_only_bounded_host_owned_fields() {
    const CANARY: &str = "CANARY_PLUGIN_LOG_CUSTOMER_SECRET";
    let message = format!("{CANARY}{}", "\0\u{1b}\n".repeat(1_024));
    let body = deny_body_with_log_message(&message);
    let (_directory, state, issued) = policy_state_with_capabilities(
        &body,
        "malicious-log",
        vec!["requested-model".into()],
        json!([{"kind": "log"}]),
    )
    .await;
    let capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(capture.clone())
        .finish();
    let dispatch = tracing::Dispatch::new(subscriber);

    let decision = tracing::dispatcher::with_default(&dispatch, || {
        state.plugins.apply_traffic(
            memeloop_token_center::plugin::memeloop::token_center::types::RequestContext {
                tenant_id: "tenant".into(),
                principal_id: "principal".into(),
                key_id: "key".into(),
                protocol: "openai".into(),
                model: "requested-model".into(),
                config_json: "{}".into(),
            },
            &json!({"model": "requested-model", "messages": []}),
        )
    })
    .expect("execute policy with Log capability");
    assert!(!decision.allow);

    let response = call(
        &state,
        &issued.key,
        "/v1/chat/completions",
        json!({"model": "requested-model", "messages": []}),
    )
    .await;

    assert_eq!(response.0, StatusCode::FORBIDDEN);
    let response_body = serde_json::to_string(&response.1).unwrap();
    let logs = capture.contents();
    assert!(!response_body.contains(CANARY), "{response_body}");
    assert!(!logs.contains(CANARY), "{logs}");
    assert!(logs.contains("gateway-policy"), "{logs}");
    assert!(logs.contains("plugin_log_emitted"), "{logs}");
    assert!(logs.len() < 4_096, "guest message amplified log output");
}

#[tokio::test]
async fn image_rewrite_rechecks_effective_permission_route_price_and_archives_charge() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .and(matches_header("authorization", "Bearer image-secret"))
        .and(body_partial_json(json!({
            "model": "image-upstream",
            "prompt": "rewritten model image"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "created": 1,
            "data": [{"b64_json": "aW1hZ2U="}]
        })))
        .expect(1)
        .mount(&mock)
        .await;
    let (_directory, state, issued) = policy_state(
        REWRITE_MODEL_BODY,
        "image-rewrite",
        vec!["requested-model".into(), "example-rewritten".into()],
    )
    .await;
    let account = create_account_and_route(
        &state,
        "policy-image-rewrite",
        "http-json",
        json!({"base_url": mock.uri(), "network_scope": "public"}),
        UpstreamCredential::ApiKey {
            value: "image-secret".into(),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
        },
        "image-upstream",
    )
    .await;
    state
        .db
        .upsert_generation_price("example-rewritten", "USD", "image", Decimal::new(5, 2))
        .await
        .unwrap();
    let response = call(
        &state,
        &issued.key,
        "/v1/images/generations",
        json!({"model": "requested-model", "prompt": "rewritten model image"}),
    )
    .await;
    assert_eq!(response.0, StatusCode::OK);
    assert_eq!(response.1["data"][0]["b64_json"], "aW1hZ2U=");

    let key = state
        .db
        .authenticate_key(&issued.key, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    for _ in 0..50 {
        let stats = state
            .db
            .stats_filtered(
                key.key_id,
                memeloop_token_center::db::StatsFilter {
                    from_created_at: Some(unix_millis().saturating_sub(60_000)),
                    to_created_at: Some(unix_millis().saturating_add(1)),
                    upstream_account_id: Some(account.id),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        if stats.summary.total_requests == 1 {
            assert_eq!(stats.summary.total_cost.as_deref(), Some("0.05"));
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("rewritten image request was not finalized");
}

#[tokio::test]
async fn async_rewrite_rechecks_seedance_and_comfyui_routes_and_billing_units() {
    for (label, driver, billing_unit, config, path, input) in [
        (
            "seedance-rewrite",
            "volcengine-seedance",
            "second",
            json!({"base_url": "https://seedance.example.test"}),
            "/v1/videos/generations",
            json!({"duration": 5, "content": [{"type": "text", "text": "video"}]}),
        ),
        (
            "comfy-rewrite",
            "comfyui",
            "job",
            json!({
                "base_url": "https://comfy.example.test",
                "workflow_id": "fixture-v1",
                "workflow_template": {"1": {"inputs": {"text": {"$mtc_param": "prompt"}}}}
            }),
            "/v1/generations",
            json!({"parameters": {"prompt": "image"}}),
        ),
    ] {
        let (_directory, state, issued) = policy_state(
            REWRITE_MODEL_BODY,
            label,
            vec!["requested-model".into(), "example-rewritten".into()],
        )
        .await;
        create_account_and_route(
            &state,
            &format!("policy-{label}"),
            driver,
            config,
            UpstreamCredential::None,
            "provider-model",
        )
        .await;
        state
            .db
            .upsert_generation_price("example-rewritten", "USD", billing_unit, Decimal::new(1, 2))
            .await
            .unwrap();
        let response = call(
            &state,
            &issued.key,
            path,
            json!({"model": "requested-model", "input": input}),
        )
        .await;
        assert_eq!(response.0, StatusCode::ACCEPTED, "{driver}");
        assert_eq!(response.1["model"], "example-rewritten");
        assert_eq!(response.1["driver"], driver);
    }
}

#[tokio::test]
async fn rewritten_model_is_checked_again_against_the_stable_key_policy() {
    let (_directory, state, issued) = policy_state(
        REWRITE_MODEL_BODY,
        "permission",
        vec!["requested-model".into()],
    )
    .await;
    let response = call(
        &state,
        &issued.key,
        "/v1/videos/generations",
        json!({"model": "requested-model", "input": {"duration": 5}}),
    )
    .await;
    assert_eq!(response.0, StatusCode::FORBIDDEN);
}

async fn create_account_and_route(
    state: &AppState,
    tenant: &str,
    driver: &str,
    config: Value,
    credential: UpstreamCredential,
    upstream_model: &str,
) -> memeloop_token_center::provider::UpstreamAccountView {
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.into(),
                name: format!("{driver}-account"),
                driver: driver.into(),
                config,
                credential,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant.into(),
            public_model: "example-rewritten".into(),
            upstream_account_id: account.id,
            upstream_model: upstream_model.into(),
            protocol: "generation".into(),
            priority: 0,
        })
        .await
        .unwrap();
    account
}

async fn call(state: &AppState, key: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = api::router_for_role(state.clone(), RuntimeRole::Gateway)
        .oneshot(
            Request::post(path)
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (status, body)
}
