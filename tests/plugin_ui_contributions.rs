use std::fs;

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::CreateServiceTokenInput,
    plugin::PluginOperatorUiPresentation,
};
use serde_json::Value;
use tower::ServiceExt;

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "fixtures/plugins/operator-ui-contributions.json"
    ))
    .expect("checked-in UI contribution fixture is JSON")
}

fn write_package(root: &std::path::Path, name: &str, manifest: &Value) {
    let package = root.join(name);
    fs::create_dir_all(&package).expect("create fixture plugin package");
    fs::write(
        package.join("plugin.json"),
        serde_json::to_vec(manifest).expect("encode fixture plugin manifest"),
    )
    .expect("write fixture plugin manifest");
    for entry in manifest["contributions"]["operator_ui"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|contribution| contribution["module_entry"].as_str())
    {
        let path = package.join(entry);
        fs::create_dir_all(path.parent().unwrap()).expect("create fixture UI module directory");
        fs::write(path, b"export function activateOperatorUi() {}\n")
            .expect("write fixture UI module");
    }
}

#[tokio::test]
async fn operator_ui_fixture_loads_and_data_permission_is_checked_before_any_outbound_call() {
    let directory = tempfile::tempdir().unwrap();
    let plugins = directory.path().join("plugins");
    let values = fixture();
    write_package(&plugins, "observability-suite", &values["installed"][0]);
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("ui.db").display()
    );
    let mut config = Config::for_test(database_url);
    config.plugin_dir = Some(plugins.display().to_string());
    let state = AppState::initialize(config)
        .await
        .expect("load UI plugin fixture");
    let mut manifests = state.plugins.manifests();
    let manifest = manifests.pop().expect("loaded fixture manifest");
    assert_eq!(manifest.contributions.operator_ui.len(), 3);
    assert_eq!(manifest.contributions.service_data.len(), 2);
    assert!(matches!(
        manifest.contributions.operator_ui[0].presentation,
        Some(PluginOperatorUiPresentation::HealthIntelligenceV1)
    ));

    let issued = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "plugins-without-metrics".into(),
                scopes: vec!["plugins:read".into()],
                tenant_external_id: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .expect("create scoped service credential");
    let response = api::router_for_role(state, RuntimeRole::Control)
        .oneshot(
            Request::get("/internal/v1/plugins/observability-suite/data/health")
                .header(header::AUTHORIZATION, format!("Bearer {}", issued.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn authorized_service_data_reads_the_durable_fallback_without_network_io() {
    let directory = tempfile::tempdir().unwrap();
    let plugins = directory.path().join("plugins");
    let values = fixture();
    write_package(&plugins, "observability-suite", &values["installed"][0]);
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("fallback.db").display()
    );
    let mut config = Config::for_test(database_url);
    config.plugin_dir = Some(plugins.display().to_string());
    let state = AppState::initialize(config)
        .await
        .expect("load UI plugin fixture");
    let issued = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "plugins-with-metrics".into(),
                scopes: vec!["plugins:read".into(), "metrics:read".into()],
                tenant_external_id: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .expect("create scoped service credential");
    let response = api::router_for_role(state, RuntimeRole::Control)
        .oneshot(
            Request::get("/internal/v1/plugins/observability-suite/data/health")
                .header(header::AUTHORIZATION, format!("Bearer {}", issued.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap()).unwrap();
    assert_eq!(
        body["data"],
        serde_json::json!({"status": "offline", "checks": []})
    );
    assert_eq!(body["partial"], true);
    assert_eq!(body["provenance"]["freshness"], "unavailable");
    assert_eq!(body["provenance"]["source"], "fallback");
    assert_eq!(body["provenance"]["last_attempt_at"], Value::Null);
}

#[tokio::test]
async fn conflicting_new_plugin_category_prevents_the_plugin_set_from_loading() {
    let directory = tempfile::tempdir().unwrap();
    let plugins = directory.path().join("plugins");
    let values = fixture();
    write_package(&plugins, "observability-suite", &values["installed"][0]);
    write_package(
        &plugins,
        "different-intelligence",
        &values["conflicting_category"],
    );
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("conflict.db").display()
    );
    let mut config = Config::for_test(database_url);
    config.plugin_dir = Some(plugins.display().to_string());
    assert!(
        AppState::initialize(config).await.is_err(),
        "conflicting category must fail closed"
    );
}

#[tokio::test]
async fn arbitrary_operator_presentation_is_rejected_before_plugin_load() {
    let directory = tempfile::tempdir().unwrap();
    let plugins = directory.path().join("plugins");
    let values = fixture();
    let mut manifest = values["installed"][0].clone();
    manifest["contributions"]["operator_ui"][0]["presentation"] =
        Value::String("remote_browser_code".into());
    write_package(&plugins, "unsupported-presentation", &manifest);
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory
            .path()
            .join("unsupported-presentation.db")
            .display()
    );
    let mut config = Config::for_test(database_url);
    config.plugin_dir = Some(plugins.display().to_string());
    assert!(
        AppState::initialize(config).await.is_err(),
        "operator presentations must remain a closed core-owned enum"
    );
}

#[tokio::test]
async fn normalized_operator_module_paths_are_rejected_by_the_public_manifest_contract() {
    let directory = tempfile::tempdir().unwrap();
    let plugins = directory.path().join("plugins");
    let values = fixture();
    let mut manifest = values["installed"][0].clone();
    manifest["contributions"]["operator_ui"][0]["renderer"] = Value::String("component_v1".into());
    manifest["contributions"]["operator_ui"][0]["module_entry"] =
        Value::String("assets//operator-ui.mjs".into());
    manifest["contributions"]["operator_ui"][0]["component_id"] = Value::String("workspace".into());
    manifest["contributions"]["operator_ui"][0]
        .as_object_mut()
        .unwrap()
        .remove("presentation");
    write_package(&plugins, "normalized-module-path", &manifest);
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("normalized-module-path.db").display()
    );
    let mut config = Config::for_test(database_url);
    config.plugin_dir = Some(plugins.display().to_string());
    assert!(
        AppState::initialize(config).await.is_err(),
        "manifest validation must reject module paths that require normalization"
    );
}

#[tokio::test]
async fn component_operator_contributions_support_tabs_and_existing_page_slots() {
    let directory = tempfile::tempdir().unwrap();
    let plugins = directory.path().join("plugins");
    let values = fixture();
    let mut manifest = values["installed"][0].clone();
    manifest["contributions"]["operator_ui"] = serde_json::json!([
        {
            "id": "interactive-tab",
            "slot": "operator.sidebar.tab",
            "category": { "id": "monitoring" },
            "route": "interactive-health",
            "label": "Interactive health",
            "icon": "heart",
            "renderer": "component_v1",
            "module_entry": "assets/operator-ui.mjs",
            "component_id": "health-workspace",
            "component_props": { "defaultRange": "24h" }
        },
        {
            "id": "provider-footer",
            "slot": "operator.page.after",
            "target_route": "providers",
            "label": "Provider intelligence",
            "icon": "chart",
            "renderer": "component_v1",
            "module_entry": "assets/operator-ui.mjs",
            "component_id": "provider-intelligence",
            "data_endpoint": "health"
        }
    ]);
    write_package(&plugins, "component-ui", &manifest);
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("component-ui.db").display()
    );
    let mut config = Config::for_test(database_url);
    config.plugin_dir = Some(plugins.display().to_string());
    let state = AppState::initialize(config)
        .await
        .expect("load component UI plugin fixture");
    let manifest = state.plugins.manifests().pop().expect("component manifest");
    assert_eq!(
        manifest.contributions.operator_ui[0].renderer,
        "component_v1"
    );
    let digest = manifest.contributions.operator_ui[0]
        .module_sha256
        .as_deref()
        .expect("runtime module digest");
    assert!(digest.starts_with("sha256:"));
    assert_eq!(
        manifest.contributions.operator_ui[1]
            .target_route
            .as_deref(),
        Some("providers")
    );
    let response = api::router_for_role(state, RuntimeRole::Control)
        .oneshot(
            Request::get(format!(
                "/ui-assets/plugins/{}/1.0.0/{digest}/assets/operator-ui.mjs",
                manifest.id
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .expect("serve runtime UI module");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), 1024).await.unwrap().as_ref(),
        b"export function activateOperatorUi() {}\n"
    );
}
