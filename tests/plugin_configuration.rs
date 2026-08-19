use std::{fs, path::Path};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{CreateKeyInput, CreateServiceTokenInput},
    model::KeyPolicy,
    plugin::memeloop::token_center::types::RequestContext,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tower::ServiceExt;
use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
use wit_parser::Resolve;

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
        .expect("embed component metadata");
    ComponentEncoder::default()
        .module(&module)
        .expect("read test core module")
        .validate(true)
        .encode()
        .expect("encode test component")
}

fn configured_component(expected_configuration: &Value) -> Vec<u8> {
    let source = include_str!("../examples/plugins/policy-rewrite/plugin.wat");
    let (prefix, after_start) = source.split_once(";; BEGIN POST-AUTH BODY").unwrap();
    let (_, suffix) = after_start.split_once(";; END POST-AUTH BODY").unwrap();
    let encoded = serde_json::to_string(expected_configuration).unwrap();
    let body = format!(
        r#"
        ;; request-context.config-json is the sixth string pair (params 10/11).
        local.get 11
        i32.const {}
        i32.ne
        if unreachable end
        local.get 10
        i32.load8_u
        i32.const 123
        i32.ne
        if unreachable end
        i32.const 256 i32.const 0 i32.store
        i32.const 260 i32.const 1 i32.store
        i32.const 264 i32.const 0 i32.store
        i32.const 276 i32.const 1 i32.store
        i32.const 280 i32.const 400 i32.store
        i32.const 284 i32.const 17 i32.store
        i32.const 288 i32.const 0 i32.store
        i32.const 300 i32.const 1 i32.store
        i32.const 304 i32.const 64 i32.store
        i32.const 308 i32.const 90 i32.store
        i32.const 256
        "#,
        encoded.len()
    );
    component_from_core_wat(&format!(
        "{prefix};; BEGIN POST-AUTH BODY{body};; END POST-AUTH BODY{suffix}"
    ))
}

fn write_plugin(root: &Path) {
    let package = root.join("configured-rewrite");
    fs::create_dir_all(&package).unwrap();
    let configured = json!({"mode": "configured"});
    fs::write(
        package.join("plugin.json"),
        serde_json::to_vec(&json!({
            "id": "configured-rewrite",
            "version": "1.0.0",
            "wit_version": "0.2.0",
            "wasm": "plugin.wasm",
            "capabilities": [],
            "contributions": {
                "request_rewrite": true,
                "configuration": {
                    "schema": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["mode"],
                        "properties": {"mode": {"type": "string", "enum": ["default", "configured", "tenant"]}}
                    },
                    "default": {"mode": "default"}
                },
                "providers": []
            }
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        package.join("plugin.wasm"),
        configured_component(&configured),
    )
    .unwrap();
}

async fn call(
    state: &AppState,
    token: &str,
    method: &str,
    uri: &str,
    idempotency_key: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    if let Some(key) = idempotency_key {
        request = request.header("Idempotency-Key", key);
    }
    let body = match body {
        Some(value) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&value).unwrap())
        }
        None => Body::empty(),
    };
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let value = serde_json::from_slice(
        &to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test]
async fn plugin_configuration_is_scoped_validated_idempotent_and_injected() {
    let directory = tempfile::tempdir().unwrap();
    let plugins = directory.path().join("plugins");
    write_plugin(&plugins);
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("plugin-config.db").display()
    );
    let mut config = Config::for_test(database_url);
    config.plugin_dir = Some(plugins.to_string_lossy().into_owned());
    let state = AppState::initialize(config).await.unwrap();
    let bootstrap = state.config.service_token.clone();
    let issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "plugin-tenant".into(),
                principal_external_id: "plugin-user".into(),
                alias: "plugin-user".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ZERO,
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

    let (status, initial) = call(
        &state,
        &bootstrap,
        "GET",
        "/internal/v1/plugins/configured-rewrite/configuration",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(initial["source"], "default");
    assert_eq!(initial["scope_version"], 0);

    let global_body = json!({"expected_version": 0, "value": {"mode": "configured"}});
    let (status, global) = call(
        &state,
        &bootstrap,
        "PUT",
        "/internal/v1/plugins/configured-rewrite/configuration",
        Some("global-config-v1"),
        Some(global_body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{global}");
    assert_eq!(global["scope_version"], 1);
    let (status, replay) = call(
        &state,
        &bootstrap,
        "PUT",
        "/internal/v1/plugins/configured-rewrite/configuration",
        Some("global-config-v1"),
        Some(global_body),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay["scope_version"], 1);

    let (status, mismatch) = call(
        &state,
        &bootstrap,
        "PUT",
        "/internal/v1/plugins/configured-rewrite/configuration",
        Some("global-config-v1"),
        Some(json!({"expected_version": 1, "value": {"mode": "default"}})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{mismatch}");
    let (status, stale) = call(
        &state,
        &bootstrap,
        "PUT",
        "/internal/v1/plugins/configured-rewrite/configuration",
        Some("global-config-stale"),
        Some(json!({"expected_version": 0, "value": {"mode": "default"}})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{stale}");

    let (status, inherited) = call(
        &state,
        &bootstrap,
        "GET",
        "/internal/v1/plugins/configured-rewrite/configuration?tenant_external_id=plugin-tenant",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(inherited["source"], "global");
    assert_eq!(inherited["scope_version"], 0);
    assert_eq!(inherited["value"]["mode"], "configured");

    let resolved = state
        .plugins
        .resolved_traffic_configurations(key.tenant_id)
        .await
        .unwrap();
    let decision = state
        .plugins
        .apply_traffic_with_config(
            RequestContext {
                tenant_id: key.tenant_id.to_string(),
                principal_id: key.principal_id.to_string(),
                key_id: key.key_id.to_string(),
                protocol: "openai".into(),
                model: "original".into(),
                config_json: "{}".into(),
            },
            &json!({"model": "original", "messages": []}),
            &resolved,
        )
        .expect("persisted effective configuration reaches the guest context");
    assert_eq!(decision.model.as_deref(), Some("example-rewritten"));

    let (status, invalid) = call(
        &state,
        &bootstrap,
        "PUT",
        "/internal/v1/plugins/configured-rewrite/configuration",
        Some("invalid-config"),
        Some(json!({"expected_version": 1, "value": {"mode": "forbidden"}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{invalid}");

    let scoped = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "tenant-plugin-operator".into(),
                scopes: vec!["plugins:read".into(), "plugins:write".into()],
                tenant_external_id: Some("plugin-tenant".into()),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let (status, _) = call(
        &state,
        &scoped.token,
        "GET",
        "/internal/v1/plugins/configured-rewrite/configuration?tenant_external_id=another-tenant",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, tenant) = call(
        &state,
        &scoped.token,
        "PUT",
        "/internal/v1/plugins/configured-rewrite/configuration",
        Some("tenant-config-v1"),
        Some(json!({"expected_version": 0, "value": {"mode": "tenant"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tenant}");
    assert_eq!(tenant["source"], "tenant");
    assert_eq!(tenant["tenant_external_id"], "plugin-tenant");
}
