//! Uses the CI-built Rust guest, never a constant-output Wasm substitute.
use memeloop_token_center::{
    db::Database,
    plugin::{
        PluginRuntime,
        routing::{GroupRoutingCandidate, GroupRoutingHealth, GroupRoutingInput},
    },
};
use serde_json::json;
use std::path::PathBuf;

#[tokio::test]
#[ignore = "requires MTC_PREFERRED_ACCOUNT_PACKAGE built in CI"]
async fn preferred_account_real_component_retains_identity_and_native_health_manifest() {
    let source = PathBuf::from(
        std::env::var("MTC_PREFERRED_ACCOUNT_PACKAGE").expect("real package required"),
    );
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("plugins");
    let package = root.join("mtc-preferred-account");
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
    assert_eq!(
        serde_json::to_value(manifest).unwrap()["contributions"]["group_routing"]["health_policy"],
        "native"
    );
    let candidate = |route: &str, account: &str, health| GroupRoutingCandidate {
        tenant_id: "tenant".into(),
        route_id: route.into(),
        account_id: account.into(),
        generation: 7,
        health,
    };
    let mut input = GroupRoutingInput {
        tenant_id: "tenant".into(),
        seed: 42,
        remaining_deadline_ms: 1000,
        config: json!({"preferred_account_ids":["blocked","preferred","outside-group"]}),
        candidates: vec![
            candidate("r0", "other", GroupRoutingHealth::Healthy),
            candidate("r1", "preferred", GroupRoutingHealth::Healthy),
            candidate("r2", "blocked", GroupRoutingHealth::HardQuota),
        ],
    };
    let plan = runtime
        .execute_group_routing_plan("mtc-preferred-account", &input)
        .unwrap();
    assert_eq!(
        plan.candidates
            .iter()
            .map(|c| c.route_id.as_str())
            .collect::<Vec<_>>(),
        vec!["r1", "r0", "r2"]
    );
    assert!(
        plan.candidates
            .iter()
            .all(|c| !c.stickiness && !c.allow_transient_probe && c.generation == 7)
    );
    input.config = json!({"preferred_account_ids":[]});
    assert_eq!(
        runtime
            .execute_group_routing_plan("mtc-preferred-account", &input)
            .unwrap()
            .candidates
            .iter()
            .map(|c| c.route_id.as_str())
            .collect::<Vec<_>>(),
        vec!["r0", "r1", "r2"]
    );
}
