//! Run against the CI-built real package, never a hand-written Wasm fixture.
use memeloop_token_center::{
    db::Database,
    plugin::{PluginRuntime, memeloop::token_center::types::RequestContext},
};
use serde_json::json;
use std::{collections::BTreeMap, path::PathBuf};

#[tokio::test]
#[ignore = "requires MTC_MODEL_GUARD_PACKAGE pointing to the built release package"]
async fn real_component_preserves_defaults_and_only_denies_configured_models() {
    let source =
        PathBuf::from(std::env::var("MTC_MODEL_GUARD_PACKAGE").expect("real package required"));
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("plugins");
    let package = root.join("mtc-model-guard");
    std::fs::create_dir_all(&package).unwrap();
    for name in ["plugin.json", "plugin.wasm"] {
        std::fs::copy(source.join(name), package.join(name)).unwrap();
    }
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("test.db").display()
    ))
    .await
    .unwrap();
    let runtime = PluginRuntime::load(root.to_str(), database).unwrap();
    assert!(runtime.provider_types().is_empty());
    let manifest = &runtime.manifests()[0];
    assert!(manifest.capabilities.is_empty());
    assert!(!manifest.contributions.request_rewrite);
    let context = || RequestContext {
        tenant_id: "tenant".into(),
        principal_id: "principal".into(),
        key_id: "key".into(),
        protocol: "openai".into(),
        model: "retired-model".into(),
        config_json: "{}".into(),
    };
    let request =
        json!({"model":"retired-model","messages":[{"role":"user","content":"unchanged"}]});
    let default = runtime.apply_traffic(context(), &request).unwrap();
    assert!(default.allow);
    assert!(default.model.is_none());
    assert!(default.upstream_account_id.is_none());
    assert!(default.request_json.is_none());
    let configured = BTreeMap::from([(
        "mtc-model-guard".into(),
        json!({"blocked_models":["retired-model"]}),
    )]);
    assert!(
        !runtime
            .apply_traffic_with_config(context(), &request, &configured)
            .unwrap()
            .allow
    );
    assert!(
        runtime.apply_traffic(context(), &request).unwrap().allow,
        "configuration cannot leak to another request"
    );
}
