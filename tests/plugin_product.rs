use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

use memeloop_token_center::{
    db::Database,
    plugin::{PluginRuntime, memeloop::token_center::types::RequestContext},
    provider::ProviderCatalog,
};
use serde_json::json;
use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
use wit_parser::Resolve;

async fn database(directory: &Path) -> Database {
    Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.join("plugin.db").display()
    ))
    .await
    .expect("connect SQLite")
}

fn context() -> RequestContext {
    RequestContext {
        tenant_id: "tenant-1".to_owned(),
        principal_id: "principal-1".to_owned(),
        key_id: "key-1".to_owned(),
        protocol: "openai".to_owned(),
        model: "requested-model".to_owned(),
        config_json: "{}".to_owned(),
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

fn core_wat_with_post_auth(body: &str) -> String {
    let source = include_str!("../examples/plugins/policy-rewrite/plugin.wat");
    let (_, after_start) = source
        .split_once(";; BEGIN POST-AUTH BODY")
        .expect("example start marker");
    let (_, suffix) = after_start
        .split_once(";; END POST-AUTH BODY")
        .expect("example end marker");
    format!(
        "{};; BEGIN POST-AUTH BODY\n{body}\n;; END POST-AUTH BODY{}",
        source.split_once(";; BEGIN POST-AUTH BODY").unwrap().0,
        suffix
    )
}

fn write_policy_package(root: &Path, id: &str, body: &str) {
    let package = root.join(id);
    fs::create_dir(&package).expect("create plugin package");
    fs::write(
        package.join("plugin.json"),
        serde_json::to_vec(&json!({
            "id": id,
            "version": "1.0.0",
            "wit_version": "0.1.0",
            "wasm": "plugin.wasm",
            "capabilities": [],
            "contributions": {"traffic_policy": true, "providers": []}
        }))
        .unwrap(),
    )
    .expect("write plugin manifest");
    fs::write(
        package.join("plugin.wasm"),
        component_from_core_wat(&core_wat_with_post_auth(body)),
    )
    .expect("write plugin component");
}

#[test]
fn checked_in_example_component_is_reproducible_from_auditable_wat() {
    assert_eq!(
        component_from_core_wat(include_str!(
            "../examples/plugins/policy-rewrite/plugin.wat"
        )),
        include_bytes!("../examples/plugins/policy-rewrite/plugin.wasm")
    );
}

#[tokio::test]
async fn installable_example_contributes_provider_oauth_policy_and_rewrite() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = PluginRuntime::load(Some("examples/plugins"), database(directory.path()).await)
        .expect("load checked-in example");

    let manifests = runtime.manifests();
    assert_eq!(manifests.len(), 1);
    assert!(manifests[0].contributions.traffic_policy);
    let provider = runtime
        .provider_types()
        .into_iter()
        .find(|provider| provider.id == "example-oauth-http")
        .expect("example provider contribution");
    assert_eq!(provider.source, "plugin:example-policy-rewrite@1.0.0");
    assert!(provider.oauth_adapter.is_some());
    assert_eq!(provider.protocols, ["openai", "anthropic"]);

    let mut catalog = ProviderCatalog::builtins();
    catalog.extend(runtime.provider_types()).unwrap();
    assert!(catalog.contains("example-oauth-http"));
    memeloop_token_center::schema::validate_instance(
        &provider.config_schema,
        &json!({"base_url": "https://api.example.com"}),
    )
    .expect("valid provider config");
    assert!(
        memeloop_token_center::schema::validate_instance(
            &provider.config_schema,
            &json!({"base_url": "not a URL"}),
        )
        .is_err()
    );
    assert_eq!(
        runtime
            .list_provider_models(
                "example-oauth-http",
                &json!({"base_url": "https://api.example.com"}),
            )
            .expect("invoke component provider discovery")
            .expect("component provider models"),
        json!(["example-rewritten"])
    );

    let original = json!({"model": "requested-model", "messages": []});
    let decision = runtime
        .apply_traffic(context(), &original)
        .expect("execute traffic policy");
    assert!(decision.allow);
    assert_eq!(decision.model.as_deref(), Some("example-rewritten"));
    assert_eq!(
        decision.request_json.unwrap()["messages"][0]["content"],
        "rewritten by plugin"
    );
}

#[tokio::test]
async fn discovery_rejects_invalid_schema_duplicate_ids_and_incompatible_upgrades() {
    let directory = tempfile::tempdir().unwrap();
    let plugins = directory.path().join("plugins");
    fs::create_dir(&plugins).unwrap();
    let package = plugins.join("invalid");
    fs::create_dir(&package).unwrap();
    fs::write(
        package.join("plugin.json"),
        r#"{
          "id":"invalid","version":"not-semver","wit_version":"0.2.0","wasm":null,
          "contributions":{"providers":[]},"unexpected":true
        }"#,
    )
    .unwrap();
    assert!(
        PluginRuntime::load(plugins.to_str(), database(directory.path()).await.clone()).is_err()
    );

    fs::remove_dir_all(&plugins).unwrap();
    fs::create_dir(&plugins).unwrap();
    let package = plugins.join("one");
    fs::create_dir(&package).unwrap();
    fs::write(
        package.join("plugin.json"),
        r#"{
          "id":"one","version":"1.2.3","wit_version":"0.1.9","wasm":null,
          "contributions":{"providers":[{
            "id":"duplicate-provider","display_name":"Duplicate","protocols":["openai"],
            "modalities":["text"],"config_schema":{"type":"object"},
            "credential_schema":{"type":"object"}
          }]}
        }"#,
    )
    .unwrap();
    let compatible =
        PluginRuntime::load(plugins.to_str(), database(directory.path()).await.clone())
            .expect("0.1 patch upgrade remains compatible");
    assert_eq!(compatible.manifests()[0].wit_version, "0.1.9");

    let package = plugins.join("two");
    fs::create_dir(&package).unwrap();
    fs::write(
        package.join("plugin.json"),
        r#"{
          "id":"two","version":"1.2.3","wit_version":"0.1.9","wasm":null,
          "contributions":{"providers":[{
            "id":"duplicate-provider","display_name":"Duplicate","protocols":["openai"],
            "modalities":["text"],"config_schema":{"type":"object"},
            "credential_schema":{"type":"object"}
          }]}
        }"#,
    )
    .unwrap();
    assert!(PluginRuntime::load(plugins.to_str(), database(directory.path()).await).is_err());

    fs::remove_dir_all(&plugins).unwrap();
    fs::create_dir(&plugins).unwrap();
    let package = plugins.join("bad-schema");
    fs::create_dir(&package).unwrap();
    fs::write(
        package.join("plugin.json"),
        r#"{
          "id":"bad-schema","version":"1.0.0","wit_version":"0.1.0","wasm":null,
          "contributions":{"providers":[{
            "id":"bad-schema-provider","display_name":"Bad schema","protocols":["openai"],
            "modalities":["text"],"config_schema":{"$ref":"https://untrusted.invalid/schema"},
            "credential_schema":{"type":"object"}
          }]}
        }"#,
    )
    .unwrap();
    assert!(PluginRuntime::load(plugins.to_str(), database(directory.path()).await).is_err());
}

#[tokio::test]
async fn fuel_exhaustion_and_guest_traps_fail_closed() {
    for (id, body) in [
        (
            "fuel-policy",
            "(loop $forever (br $forever))\ni32.const 256",
        ),
        ("trap-policy", "unreachable"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let plugins = directory.path().join("plugins");
        fs::create_dir(&plugins).unwrap();
        write_policy_package(&plugins, id, body);
        let runtime = PluginRuntime::load(plugins.to_str(), database(directory.path()).await)
            .expect("load failure fixture");
        let error = runtime
            .apply_traffic(context(), &json!({"model": "test"}))
            .expect_err("plugin failure must reject the request");
        assert!(error.to_string().contains(id));
    }
}

#[tokio::test]
async fn a_traffic_policy_can_explicitly_deny_after_core_authentication() {
    let directory = tempfile::tempdir().unwrap();
    let plugins = directory.path().join("plugins");
    fs::create_dir(&plugins).unwrap();
    write_policy_package(
        &plugins,
        "deny-policy",
        r#"
        i32.const 256 i32.const 0 i32.store
        i32.const 260 i32.const 0 i32.store
        i32.const 264 i32.const 0 i32.store
        i32.const 276 i32.const 0 i32.store
        i32.const 288 i32.const 0 i32.store
        i32.const 300 i32.const 0 i32.store
        i32.const 256
        "#,
    );
    let runtime = PluginRuntime::load(plugins.to_str(), database(directory.path()).await)
        .expect("load deny fixture");
    let decision = runtime
        .apply_traffic(context(), &json!({"model": "test"}))
        .expect("execute deny policy");
    assert!(!decision.allow);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn epoch_timeout_interrupts_a_guest_even_with_effectively_unlimited_fuel() {
    let directory = tempfile::tempdir().unwrap();
    let plugins = directory.path().join("plugins");
    fs::create_dir(&plugins).unwrap();
    write_policy_package(
        &plugins,
        "timeout-policy",
        "(loop $forever (br $forever))\ni32.const 256",
    );
    let mut runtime = PluginRuntime::load(plugins.to_str(), database(directory.path()).await)
        .expect("load timeout fixture");
    runtime.set_execution_limits_for_tests(Duration::from_millis(30), u64::MAX);

    let started = Instant::now();
    let error = runtime
        .apply_traffic(context(), &json!({"model": "test"}))
        .expect_err("execution timeout must reject the request");
    assert!(error.to_string().contains("timeout-policy"));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "epoch deadline was not enforced"
    );
}
