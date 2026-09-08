use std::fs;

use axum::{
    body::Body,
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
        directory.path().join("unsupported-presentation.db").display()
    );
    let mut config = Config::for_test(database_url);
    config.plugin_dir = Some(plugins.display().to_string());
    assert!(
        AppState::initialize(config).await.is_err(),
        "operator presentations must remain a closed core-owned enum"
    );
}
