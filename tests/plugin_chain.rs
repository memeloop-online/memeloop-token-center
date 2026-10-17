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

    #[cfg(feature = "experimental-plugin-revisions")]
    {
        use memeloop_token_center::plugin::lifecycle::{
            PluginGrant, RuntimeRevisions, manifest_digest,
        };
        let identities = runtime.package_identities();
        let grants = runtime
            .manifests()
            .into_iter()
            .map(|manifest| {
                let grant = PluginGrant {
                    version: manifest.version.clone(),
                    capabilities: manifest.capabilities.clone(),
                    manifest_digest: manifest_digest(&manifest).unwrap(),
                    identity: identities[&manifest.id].clone(),
                };
                (manifest.id, vec![grant])
            })
            .collect::<BTreeMap<_, _>>();
        let revisions = RuntimeRevisions::new(runtime, grants).unwrap();
        let decision = revisions
            .pin()
            .unwrap()
            .apply_traffic_with_config(original_context.clone(), &original, &BTreeMap::new())
            .unwrap();
        assert_decision(&decision, allow, explicit, label);
        if allow {
            assert_eq!(decision.request_json.as_ref(), last_rewrite, "{label}");
        }
    }
    assert_eq!(original_context.model, "requested-model");
    assert_eq!(
        original,
        json!({"model": "requested-model", "marker": "keep-input"})
    );
}

#[tokio::test]
async fn downstream_policy_observes_effective_model_and_canonical_body_in_both_executors() {
    check_chain(
        "allow without rewrite",
        &[(None, None)],
        "requested-model",
        true,
    )
    .await;
    check_chain(
        "deny without rewrite",
        &[(None, None)],
        "requested-model",
        false,
    )
    .await;
    check_chain(
        "explicit model",
        &[(Some("blocked-model"), None)],
        "blocked-model",
        false,
    )
    .await;
    check_chain(
        "body model",
        &[(
            None,
            Some(json!({"model": "blocked-model", "marker": "rewritten"})),
        )],
        "blocked-model",
        false,
    )
    .await;
    check_chain(
        "explicit beats conflicting body",
        &[(
            Some("allowed-model"),
            Some(json!({"model": "blocked-model"})),
        )],
        "allowed-model",
        true,
    )
    .await;
    check_chain(
        "earlier explicit beats later body",
        &[
            (Some("blocked-model"), None),
            (None, Some(json!({"model": "allowed-model"}))),
        ],
        "blocked-model",
        false,
    )
    .await;
    check_chain(
        "later explicit replaces earlier explicit",
        &[(Some("allowed-model"), None), (Some("blocked-model"), None)],
        "blocked-model",
        false,
    )
    .await;
    for invalid in [Value::Null, json!(17), json!(" "), json!("x".repeat(201))] {
        check_chain(
            "invalid body model falls back to original",
            &[(None, Some(json!({"model": invalid})))],
            "requested-model",
            true,
        )
        .await;
    }
    check_chain(
        "missing body model falls back to original",
        &[(None, Some(json!({"marker": "rewritten"})))],
        "requested-model",
        true,
    )
    .await;
}

const TRAFFIC_INPUT_LIMIT: usize = 16 * 1024 * 1024;

#[cfg(feature = "experimental-plugin-revisions")]
fn revisions(runtime: PluginRuntime) -> memeloop_token_center::plugin::lifecycle::RuntimeRevisions {
    use memeloop_token_center::plugin::lifecycle::{
        PluginGrant, RuntimeRevisions, manifest_digest,
    };
    let identities = runtime.package_identities();
    let grants = runtime
        .manifests()
        .into_iter()
        .map(|manifest| {
            let grant = PluginGrant {
                version: manifest.version.clone(),
                capabilities: manifest.capabilities.clone(),
                manifest_digest: manifest_digest(&manifest).unwrap(),
                identity: identities[&manifest.id].clone(),
            };
            (manifest.id, vec![grant])
        })
        .collect();
    RuntimeRevisions::new(runtime, grants).unwrap()
}

#[tokio::test]
async fn traffic_input_exact_total_boundary_and_escaping_are_request_errors() {
    use memeloop_token_center::error::AppError;
    let directory = tempfile::tempdir().unwrap();
    let plugins = directory.path().join("plugins");
    fs::create_dir(&plugins).unwrap();
    write_policy_with_memory(&plugins, "policy", &decision_body(true, None, None), true);
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("input.db").display()
    ))
    .await
    .unwrap();
    let runtime = PluginRuntime::load(plugins.to_str(), database).unwrap();
    let ctx = context();
    let fields = [
        &ctx.tenant_id,
        &ctx.principal_id,
        &ctx.key_id,
        &ctx.protocol,
        &ctx.model,
    ];
    let common = fields.iter().map(|field| field.len()).sum::<usize>() + 2; // actual configuration {}
    let mut exact = json!({"model":ctx.model, "input":""});
    let overhead = common + serde_json::to_vec(&exact).unwrap().len();
    exact["input"] = json!("x".repeat(TRAFFIC_INPUT_LIMIT - overhead));
    let mut below = exact.clone();
    below["input"] = json!("x".repeat(TRAFFIC_INPUT_LIMIT - overhead - 1));
    assert!(runtime.apply_traffic(ctx.clone(), &below).unwrap().allow);
    assert!(runtime.apply_traffic(ctx.clone(), &exact).unwrap().allow);
    let mut above = exact.clone();
    above["input"] = json!(format!("{}x", above["input"].as_str().unwrap()));
    assert!(matches!(
        runtime.apply_traffic(ctx.clone(), &above),
        Err(AppError::PluginInputTooLarge)
    ));
    let escaped = json!({"model":ctx.model, "input":"\0".repeat(TRAFFIC_INPUT_LIMIT / 6 + 1)});
    assert!(escaped["input"].as_str().unwrap().len() < TRAFFIC_INPUT_LIMIT);
    assert!(matches!(
        runtime.apply_traffic(ctx.clone(), &escaped),
        Err(AppError::PluginInputTooLarge)
    ));
    // UTF-8 bytes (not char count) and a larger actual configuration both count.
    let unicode = json!({"model":ctx.model, "input":"界".repeat(TRAFFIC_INPUT_LIMIT / 3 + 1)});
    assert!(matches!(
        runtime.apply_traffic(ctx.clone(), &unicode),
        Err(AppError::PluginInputTooLarge)
    ));
    let mut larger_context = ctx.clone();
    larger_context.principal_id.push('界');
    assert!(matches!(
        runtime.apply_traffic(larger_context, &exact),
        Err(AppError::PluginInputTooLarge)
    ));
    let configs = std::collections::BTreeMap::from([("policy".into(), json!({"setting":"界"}))]);
    assert!(matches!(
        runtime.apply_traffic_with_config(ctx.clone(), &exact, &configs),
        Err(AppError::PluginInputTooLarge)
    ));
    #[cfg(feature = "experimental-plugin-revisions")]
    {
        let revisions = revisions(runtime);
        let snapshot = revisions.pin().unwrap();
        // Repeated rejections must not be swallowed as403 or count toward the
        // shared circuit; another tenant must still execute its policy.
        for _ in 0..4 {
            assert!(matches!(
                snapshot.apply_traffic_with_config(ctx.clone(), &above, &BTreeMap::new()),
                Err(AppError::PluginInputTooLarge)
            ));
        }
        let mut other = ctx.clone();
        other.tenant_id = "tenant-2".into();
        assert!(
            snapshot
                .apply_traffic_with_config(
                    other,
                    &json!({"model":"requested-model","input":"small"}),
                    &BTreeMap::new()
                )
                .unwrap()
                .allow
        );
        assert!(
            snapshot
                .apply_traffic_with_config(ctx, &exact, &BTreeMap::new())
                .unwrap()
                .allow
        );
    }
}

#[tokio::test]
async fn all_hook_configs_are_checked_before_any_guest_executes() {
    use memeloop_token_center::error::AppError;
    let directory = tempfile::tempdir().unwrap();
    let plugins = directory.path().join("plugins");
    fs::create_dir(&plugins).unwrap();
    write_policy(&plugins, "a-first-would-trap", "unreachable");
    write_policy(&plugins, "z-second", &decision_body(true, None, None));
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("configs.db").display()
    ))
    .await
    .unwrap();
    let runtime = PluginRuntime::load(plugins.to_str(), database).unwrap();
    let configs = std::collections::BTreeMap::from([(
        "z-second".into(),
        json!({"setting":"x".repeat(TRAFFIC_INPUT_LIMIT)}),
    )]);
    let request = json!({"model":"requested-model"});
    assert!(matches!(
        runtime.apply_traffic_with_config(context(), &request, &configs),
        Err(AppError::PluginInputTooLarge)
    ));
    #[cfg(feature = "experimental-plugin-revisions")]
    {
        let revisions = revisions(runtime);
        assert!(matches!(
            revisions
                .pin()
                .unwrap()
                .apply_traffic_with_config(context(), &request, &configs),
            Err(AppError::PluginInputTooLarge)
        ));
    }
}

#[tokio::test]
async fn no_traffic_hooks_do_not_impose_a_component_input_limit() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("empty.db").display()
    ))
    .await
    .unwrap();
    let runtime = PluginRuntime::load(None, database).unwrap();
    let request = json!({"model":"requested-model", "input":"x".repeat(TRAFFIC_INPUT_LIMIT + 1)});
    assert!(runtime.apply_traffic(context(), &request).unwrap().allow);
    #[cfg(feature = "experimental-plugin-revisions")]
    {
        let revisions = revisions(runtime);
        assert!(
            revisions
                .pin()
                .unwrap()
                .apply_traffic_with_config(context(), &request, &BTreeMap::new())
                .unwrap()
                .allow
        );
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
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("traffic plugin input limit")
    );
}

#[tokio::test]
async fn rewritten_input_is_rechecked_before_the_next_hook_without_poisoning_circuits() {
    use memeloop_token_center::error::AppError;
    let directory = tempfile::tempdir().unwrap();
    let plugins = directory.path().join("plugins");
    fs::create_dir(&plugins).unwrap();
    let pointer = 65536;
    let length = TRAFFIC_INPUT_LIMIT;
    let prefix = stores(pointer, "{\"model\":\"requested-model\",\"input\":\"");
    let suffix = stores(pointer + length - 2, "\"}");
    let allow = decision_body(true, None, None);
    let body = format!(
        r#"
        i32.const 256 memory.grow drop
        i32.const {pointer} i32.const 120 i32.const {length} memory.fill
        {prefix}
        {suffix}
        {allow}
        drop
        i32.const 300 i32.const 1 i32.store
        i32.const 304 i32.const {pointer} i32.store
        i32.const 308 i32.const {length} i32.store
        i32.const 256
    "#
    );
    write_policy(&plugins, "a-rewriter", &body);
    write_policy(&plugins, "z-must-not-run", "unreachable");
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("rewrite.db").display()
    ))
    .await
    .unwrap();
    let mut runtime = PluginRuntime::load(plugins.to_str(), database).unwrap();
    // This synthetic rewriter bulk-fills16MiB to construct the boundary output.
    // Isolate input admission from that fixture's separately tested fuel cost;
    // production limits and all real policy-component tests stay unchanged.
    runtime.set_execution_limits_for_tests(std::time::Duration::from_secs(30), u64::MAX);
    let request = json!({"model":"requested-model"});
    assert!(matches!(
        runtime.apply_traffic(context(), &request),
        Err(AppError::PluginInputTooLarge)
    ));
    #[cfg(feature = "experimental-plugin-revisions")]
    {
        let revisions = revisions(runtime);
        let snapshot = revisions.pin().unwrap();
        for _ in 0..4 {
            assert!(matches!(
                snapshot.apply_traffic_with_config(context(), &request, &BTreeMap::new()),
                Err(AppError::PluginInputTooLarge)
            ));
        }
    }
}
