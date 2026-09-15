//! Shared deterministic projection, executed by the CI-built real Rust guest.
use memeloop_token_center::{
    db::Database,
    plugin::{PluginRuntime, routing::GroupRoutingInput},
};
use serde_json::{Value, json};
use std::path::PathBuf;

#[tokio::test]
#[ignore = "requires MTC_SOONEST_RESET_PACKAGE built in CI"]
async fn soonest_reset_real_component_orders_only_fresh_eligible_slots() {
    let source =
        PathBuf::from(std::env::var("MTC_SOONEST_RESET_PACKAGE").expect("real package required"));
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("plugins");
    let package = root.join("mtc-soonest-reset");
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
    let manifest = serde_json::to_value(&runtime.manifests()[0]).unwrap();
    assert_eq!(
        manifest["capabilities"],
        json!([{"kind":"group_routing_quota"}])
    );
    assert_eq!(
        manifest["contributions"]["group_routing"]["health_policy"],
        "native"
    );
    assert!(runtime.provider_types().is_empty());
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/soonest-reset-v1.json")).unwrap();
    let mut input: GroupRoutingInput = serde_json::from_value(fixture["input"].clone()).unwrap();
    let plan = runtime
        .execute_group_routing_plan("mtc-soonest-reset", &input)
        .unwrap();
    assert_eq!(
        json!(
            plan.candidates
                .iter()
                .map(|c| &c.route_id)
                .collect::<Vec<_>>()
        ),
        fixture["expected_route_ids"]
    );
    assert!(
        plan.candidates
            .iter()
            .all(|c| !c.stickiness && !c.allow_transient_probe && c.generation == 7)
    );
    input.config = json!({"target_provider":"","target_window_id":""});
    let native = runtime
        .execute_group_routing_plan("mtc-soonest-reset", &input)
        .unwrap();
    assert_eq!(
        native
            .candidates
            .iter()
            .map(|c| &c.route_id)
            .collect::<Vec<_>>(),
        input
            .candidates
            .iter()
            .map(|c| &c.route_id)
            .collect::<Vec<_>>()
    );
}
