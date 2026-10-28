#[cfg(feature = "experimental-plugin-revisions")]
use std::collections::BTreeMap;
use std::{fs, path::Path};

use memeloop_token_center::{
    db::Database,
    plugin::{PluginRuntime, TrafficDecision, memeloop::token_center::types::RequestContext},
};
use serde_json::{Value, json};
use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
use wit_parser::Resolve;

fn context() -> RequestContext {
    RequestContext {
        tenant_id: "tenant-1".into(),
        principal_id: "principal-1".into(),
        key_id: "key-1".into(),
        protocol: "openai".into(),
        model: "requested-model".into(),
        config_json: "{}".into(),
    }
}

fn stores(pointer: usize, value: &str) -> String {
    value
        .bytes()
        .enumerate()
        .map(|(offset, byte)| format!("i32.const {} i32.const {byte} i32.store8", pointer + offset))
        .collect::<Vec<_>>()
        .join("\n")
}

fn decision_body(allow: bool, model: Option<&str>, request: Option<&Value>) -> String {
    let request = request.map(|value| serde_json::to_string(value).unwrap());
    format!(
        r#"
        {model_stores}
        {request_stores}
        i32.const 256 i32.const 0 i32.store
        i32.const 260 i32.const {allow} i32.store
        i32.const 264 i32.const 0 i32.store
        i32.const 276 i32.const {has_model} i32.store
        i32.const 280 i32.const 4096 i32.store
        i32.const 284 i32.const {model_length} i32.store
        i32.const 288 i32.const 0 i32.store
        i32.const 300 i32.const {has_request} i32.store
        i32.const 304 i32.const 8192 i32.store
        i32.const 308 i32.const {request_length} i32.store
        i32.const 256
        "#,
        model_stores = stores(4096, model.unwrap_or_default()),
        request_stores = stores(8192, request.as_deref().unwrap_or_default()),
        allow = u8::from(allow),
        has_model = u8::from(model.is_some()),
        model_length = model.unwrap_or_default().len(),
        has_request = u8::from(request.is_some()),
        request_length = request.as_deref().unwrap_or_default().len(),
    )
}

// Inspect the real canonical ABI arguments inside a guest. A stale context or
// inconsistent body traps, which must never be mistaken for a policy denial.
fn assert_argument(pointer_parameter: usize, expected: &str) -> String {
    let mut body = format!(
        "local.get {} i32.const {} i32.ne if unreachable end\n",
        pointer_parameter + 1,
        expected.len()
    );
    for (offset, byte) in expected.bytes().enumerate() {
        body.push_str(&format!(
            "local.get {pointer_parameter} i32.load8_u offset={offset} i32.const {byte} i32.ne if unreachable end\n"
        ));
    }
    body
}

fn observer_body(model: &str, request: &Value, allow: bool) -> String {
    let mut body = String::new();
    for (parameter, expected) in [
        (0, "tenant-1"),
        (2, "principal-1"),
        (4, "key-1"),
        (6, "openai"),
        (8, model),
        (10, "{}"),
        (12, &serde_json::to_string(request).unwrap()),
    ] {
        body.push_str(&assert_argument(parameter, expected));
    }
    body.push_str(&decision_body(allow, None, None));
    body
}

fn write_policy(root: &Path, id: &str, body: &str) {
    write_policy_with_memory(root, id, body, false);
}

fn write_policy_with_memory(root: &Path, id: &str, body: &str, large_memory: bool) {
    let source = include_str!("../.plugin-examples/examples/plugins/policy-rewrite/plugin.wat");
    let (prefix, tail) = source.split_once(";; BEGIN POST-AUTH BODY").unwrap();
    let (_, suffix) = tail.split_once(";; END POST-AUTH BODY").unwrap();
    let source = format!("{prefix}{body}{suffix}");
    let source = if large_memory {
        source.replace("(memory $memory 1)", "(memory $memory 512)")
    } else {
        source
    };
    let mut module = wat::parse_str(source).unwrap();
    let mut resolve = Resolve::default();
    let (package, _) = resolve.push_path("wit/token-center.wit").unwrap();
    let world = resolve.select_world(&[package], Some("plugin")).unwrap();
    embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8).unwrap();
    let component = ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap();
    let package = root.join(id);
    fs::create_dir(&package).unwrap();
    fs::write(package.join("plugin.wasm"), component).unwrap();
    fs::write(
        package.join("plugin.json"),
        serde_json::to_vec(&json!({
            "id": id, "version": "1.0.0", "wit_version": "0.2.0",
            "wasm": "plugin.wasm", "capabilities": [],
            "contributions": {"traffic_policy": true}
        }))
        .unwrap(),
    )
    .unwrap();
    #[cfg(feature = "experimental-plugin-revisions")]
    fs::write(
        package.join(".mtc-oci-install.json"),
        serde_json::to_vec(&json!({
            "format_version": 1, "source": "registry.example/chain-fixture",
            "digest": format!("sha256:{}", "a".repeat(64)),
            "signature_policy": "cosign-public-key"
        }))
        .unwrap(),
    )
    .unwrap();
}

fn assert_decision(decision: &TrafficDecision, allow: bool, explicit: Option<&str>, label: &str) {
    assert_eq!(decision.allow, allow, "{label}: {decision:?}");
    if allow {
        assert_eq!(decision.model.as_deref(), explicit, "{label}");
    } else {
        assert!(
            format!("{decision:?}").contains("policy_denied\""),
            "{label}: a guest trap/circuit failure must not count as a policy denial: {decision:?}"
        );
    }
}

async fn check_chain(
    label: &str,
    rewrites: &[(Option<&str>, Option<Value>)],
    expected: &str,
    allow: bool,
) {
    let directory = tempfile::tempdir().unwrap();
    let plugins = directory.path().join("plugins");
    fs::create_dir(&plugins).unwrap();
    let original = json!({"model": "requested-model", "marker": "keep-input"});
    let mut body = original.clone();
    let mut explicit = None;
    for (index, (model, request)) in rewrites.iter().enumerate() {
        write_policy(
            &plugins,
            &format!("a-rewriter-{index}"),
            &decision_body(true, *model, request.as_ref()),
        );
        if let Some(request) = request {
            body = request.clone();
        }
        if model.is_some() {
            explicit = *model;
        }
    }
    body["model"] = json!(expected);
    write_policy(
        &plugins,
        "z-model-policy",
        &observer_body(expected, &body, allow),
    );
    // A deny must stop the chain even if another guest would trap.
    if !allow {
        write_policy(&plugins, "zz-after-deny", "unreachable");
    }
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("plugin.db").display()
    ))
    .await
    .unwrap();
    let runtime = PluginRuntime::load(plugins.to_str(), database).unwrap();
    let original_context = context();
    let decision = runtime
        .apply_traffic(original_context.clone(), &original)
        .unwrap();
    assert_decision(&decision, allow, explicit, label);
    let last_rewrite = rewrites
        .iter()
        .rev()
        .find_map(|(_, request)| request.as_ref());
    if allow {
        assert_eq!(decision.request_json.as_ref(), last_rewrite, "{label}");
    }
}

#[tokio::test]
async fn traffic_input_error_has_fixed_413_public_response() {
    use axum::{body::to_bytes, http::StatusCode, response::IntoResponse};
    use memeloop_token_center::error::AppError;
    let response = AppError::PluginInputTooLarge.into_response();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(body["error"]["code"], "plugin_input_too_large");
    assert!(
        body["error"]["message_en"]
            .as_str()
            .unwrap()
            .contains("traffic plugin input limit")
    );
}

#[tokio::test]
async fn rewritten_input_is_rechecked_before_the_next_hook_without_poisoning_circuits() {
    let directory = tempfile::tempdir().unwrap();
    let plugins = directory.path().join("plugins");
    fs::create_dir(&plugins).unwrap();
    let rewritten = json!({"model":"requested-model", "input":"x".repeat(512)});
    write_policy(
        &plugins,
        "a-rewriter",
        &decision_body(true, None, Some(&rewritten)),
    );
    write_policy_with_memory(&plugins, "z-next", &decision_body(true, None, None), true);
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("rewrite.db").display()
    ))
    .await
    .unwrap();
    let runtime = PluginRuntime::load(plugins.to_str(), database).unwrap();
    let request = json!({"model":"requested-model"});
    let outcome = runtime.apply_traffic(context(), &request).unwrap();
    assert!(outcome.allow);
}

#[tokio::test]
async fn model_projection_invalidates_the_cached_json_input_size() {
    let directory = tempfile::tempdir().unwrap();
    let plugins = directory.path().join("plugins");
    fs::create_dir(&plugins).unwrap();
    let model = "m".repeat(200);
    write_policy_with_memory(
        &plugins,
        "a-model",
        &decision_body(true, Some(&model), None),
        true,
    );
    write_policy_with_memory(&plugins, "z-next", &decision_body(true, None, None), true);
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("model.db").display()
    ))
    .await
    .unwrap();
    let runtime = PluginRuntime::load(plugins.to_str(), database).unwrap();
    let ctx = context();
    let request = json!({"model":ctx.model, "input":"x".repeat(48 * 1024 * 1024)});
    // The raw runtime may surface the guest's own allocation failure; the
    // lifecycle layer above it fails open either way.
    match runtime.apply_traffic(ctx.clone(), &request) {
        Ok(decision) => assert!(decision.allow),
        Err(error) => {
            assert!(format!("{error:?}").contains("plugin_execution_failed"))
        }
    }
}
