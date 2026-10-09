use super::*;
use std::{path::PathBuf, sync::Arc, time::Duration};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
use wit_parser::Resolve;

use crate::plugin::application::{PublishApplicationPlugin, RegisterEmptyInventory, installation};
use crate::{
    AppState,
    config::{Config, RuntimeRole},
};

async fn fixture() -> (tempfile::TempDir, AppState) {
    let directory = tempfile::tempdir().unwrap();
    let inventory = directory.path().join("inventory.json");
    std::fs::write(&inventory, b"{}").unwrap();
    let mut config = Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("export.db").display()
    ));
    config.plugin_inventory_file = Some(inventory.to_str().unwrap().into());
    let state = AppState::initialize(config).await.unwrap();
    (directory, state)
}

async fn call(state: &AppState, token: &str, body: Value) -> (StatusCode, Value) {
    let response = crate::api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::post("/internal/v1/plugin-runtime/descriptor")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    if status.is_success() {
        assert_eq!(response.headers()["cache-control"], "private, no-store");
    }
    let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn export(state: &AppState, inventory_id: Option<&str>, expected_revision: i64) -> Value {
    let (status, value) = call(
        state,
        &state.config.service_token,
        json!({
            "inventory_id": inventory_id, "expected_revision": expected_revision
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    let bytes = serde_json::to_vec(&value["descriptor"]).unwrap();
    PluginInventoryDescriptor::parse_expected(&bytes, value["descriptor_digest"].as_str().unwrap())
        .unwrap();
    value
}

async fn register_empty(state: &AppState, inventory_id: &str) {
    state
        .application_plugins
        .as_ref()
        .unwrap()
        .register_empty(
            RegisterEmptyInventory {
                inventory_id: inventory_id.into(),
            },
            &format!("register-{inventory_id}"),
            "bootstrap",
        )
        .await
        .unwrap();
}

async fn publish(state: &AppState, inventory_id: &str, expected_revision: i64) {
    state
        .application_plugins
        .as_ref()
        .unwrap()
        .publish(
            PublishApplicationPlugin {
                inventory_id: inventory_id.into(),
                expected_revision,
            },
            &format!("publish-{expected_revision}"),
        )
        .await
        .unwrap();
}

async fn register_nonempty(state: &AppState, directory: &Path) -> PathBuf {
    let root = directory.join("source-release");
    let package = root.join("export-fixture");
    std::fs::create_dir_all(&package).unwrap();
    let mut manifest: Value = serde_json::from_str(include_str!(
        "../../../../examples/plugins/policy-rewrite/plugin.json"
    ))
    .unwrap();
    manifest["id"] = json!("export-fixture");
    std::fs::write(
        package.join("plugin.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let mut module = wat::parse_str(include_str!(
        "../../../../examples/plugins/policy-rewrite/plugin.wat"
    ))
    .unwrap();
    let mut resolve = Resolve::default();
    let (package_id, _) = resolve.push_path("wit/token-center.wit").unwrap();
    let world = resolve.select_world(&[package_id], Some("plugin")).unwrap();
    embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8).unwrap();
    let component = ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap();
    std::fs::write(package.join("plugin.wasm"), component).unwrap();
    std::fs::write(
        package.join(".mtc-oci-install.json"),
        serde_json::to_vec(&json!({
            "format_version":1, "source":"ghcr.io/example/export-fixture",
            "digest":format!("sha256:{}", "a".repeat(64)), "signature_policy":"cosign-public-key"
        }))
        .unwrap(),
    )
    .unwrap();
    let runtime = PluginRuntime::load(root.to_str(), state.db.clone()).unwrap();
    let manifest = runtime.manifests().into_iter().next().unwrap();
    let identity = runtime.package_identities()["export-fixture"].clone();
    let entry = PreinstalledInventory {
        root,
        grants: BTreeMap::from([(
            "export-fixture".into(),
            vec![lifecycle::PluginGrant {
                version: manifest.version.clone(),
                capabilities: manifest.capabilities.clone(),
                manifest_digest: lifecycle::manifest_digest(&manifest).unwrap(),
                identity,
            }],
        )]),
    };
    installation::append_inventory_file(
        Path::new(state.config.plugin_inventory_file.as_ref().unwrap()),
        "release",
        &entry,
    )
    .await
    .unwrap();
    state
        .application_plugins
        .as_ref()
        .unwrap()
        .stage("release")
        .await
        .unwrap();
    package
}

#[tokio::test]
async fn descriptor_export_handles_no_head_empty_head_and_zero_installations() {
    let (directory, state) = fixture().await;
    let before = state
        .db
        .plugin_descriptor_authority_snapshot()
        .await
        .unwrap();
    let absent = export(&state, None, 0).await;
    assert!(absent["observed_head"].is_null());
    assert_eq!(absent["descriptor"]["inventories"], json!({}));
    assert_eq!(
        state
            .db
            .plugin_descriptor_authority_snapshot()
            .await
            .unwrap(),
        before
    );
    register_empty(&state, "empty").await;
    let candidate = export(&state, Some("empty"), 0).await;
    assert!(candidate["observed_head"].is_null());
    let entry = &candidate["descriptor"]["inventories"]["empty"];
    assert_eq!(entry["inventory"]["grants"], json!({}));
    assert_eq!(entry["packages"], json!({}));
    assert_eq!(
        entry["inventory"]["root"],
        json!(directory.path().join("inventory-empty"))
    );
    assert!(!directory.path().join("inventory-empty").exists());
    assert!(state.db.plugin_installations().await.unwrap().is_empty());
    publish(&state, "empty", 0).await;
    let published = export(&state, Some("empty"), 1).await;
    assert_eq!(published["observed_head"]["revision"], 1);
    assert_eq!(
        published["descriptor_digest"],
        candidate["descriptor_digest"]
    );
    assert!(state.db.plugin_installations().await.unwrap().is_empty());
    assert_eq!(state.db.plugin_audit(None).await.unwrap().len(), 2);
}

#[tokio::test]
async fn descriptor_export_nonempty_history_is_stable_and_preserves_host_grants() {
    let (directory, mut state) = fixture().await;
    register_nonempty(&state, directory.path()).await;
    let destination = directory.path().join("trusted-destination");
    let mut config = (*state.config).clone();
    config.plugin_dir = Some(destination.to_str().unwrap().into());
    state.config = Arc::new(config);
    let first = export(&state, Some("release"), 0).await;
    let descriptor =
        PluginInventoryDescriptor::parse(&serde_json::to_vec(&first["descriptor"]).unwrap())
            .unwrap();
    let entry = &descriptor.inventories["release"];
    assert!(entry.packages["export-fixture"].component_sha256.is_some());
    assert_eq!(entry.inventory.root, destination.join("inventory-release"));
    assert_eq!(
        entry.package_references().unwrap()["export-fixture"],
        format!("ghcr.io/example/export-fixture@sha256:{}", "a".repeat(64))
    );
    let authority = state.application_plugins.as_ref().unwrap();
    let host_grants = authority.inventory.read().await["release"].grants.clone();
    assert_eq!(
        serde_json::to_value(&entry.inventory.grants).unwrap(),
        serde_json::to_value(host_grants).unwrap()
    );
    register_empty(&state, "later").await;
    let second = export(&state, Some("later"), 0).await;
    let extended =
        PluginInventoryDescriptor::parse(&serde_json::to_vec(&second["descriptor"]).unwrap())
            .unwrap();
    extended.validate_extension_of(&descriptor).unwrap();
    assert_ne!(first["descriptor_digest"], second["descriptor_digest"]);
    assert!(!destination.exists());
}

#[tokio::test]
async fn descriptor_export_rejects_tampered_provenance_without_changing_warm_forwarding() {
    let (directory, state) = fixture().await;
    let package = register_nonempty(&state, directory.path()).await;
    publish(&state, "release", 0).await;
    export(&state, Some("release"), 1).await;
    let authority = state.application_plugins.as_ref().unwrap();
    let warm = authority.pin().await.unwrap();
    let before = state
        .db
        .plugin_descriptor_authority_snapshot()
        .await
        .unwrap();
    let receipt = package.join(".mtc-oci-install.json");
    let original = std::fs::read(&receipt).unwrap();
    std::fs::remove_file(&receipt).unwrap();
    let (status, _) = call(
        &state,
        &state.config.service_token,
        json!({"inventory_id":"release","expected_revision":1}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let mut corrupted: Value = serde_json::from_slice(&original).unwrap();
    corrupted["digest"] = json!(format!("sha256:{}", "b".repeat(64)));
    std::fs::write(&receipt, serde_json::to_vec(&corrupted).unwrap()).unwrap();
    let (status, _) = call(
        &state,
        &state.config.service_token,
        json!({"inventory_id":"release","expected_revision":1}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        state
            .db
            .plugin_descriptor_authority_snapshot()
            .await
            .unwrap(),
        before
    );
    let forwarded = state.clone().pin_application_plugins().await.unwrap();
    assert!(Arc::ptr_eq(
        &warm,
        forwarded.pinned_application_plugins.as_ref().unwrap()
    ));
    std::fs::write(&receipt, original).unwrap();
    let pool = sqlx::AnyPool::connect(&state.config.database_url)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE application_plugin_candidates SET contract_digest=$1 WHERE inventory_id='release'",
    )
    .bind("f".repeat(64))
    .execute(&pool)
    .await
    .unwrap();
    let (status, _) = call(
        &state,
        &state.config.service_token,
        json!({"inventory_id":"release","expected_revision":1}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    pool.close().await;
}

#[tokio::test]
async fn descriptor_export_reads_all_history_beyond_one_hundred() {
    let (_directory, state) = fixture().await;
    register_empty(&state, "oldest").await;
    register_empty(&state, "newest").await;
    for expected in 0..105 {
        state
            .db
            .publish_application_plugin(
                if expected == 0 { "oldest" } else { "newest" },
                expected,
                if expected == 0 { "initial" } else { "reload" },
                &format!("history-{expected}"),
                &plugin_configuration_schema_digest(&json!(expected)).unwrap(),
                "bootstrap",
            )
            .await
            .unwrap();
    }
    assert_eq!(
        state
            .db
            .application_plugin_history(None)
            .await
            .unwrap()
            .len(),
        100
    );
    let snapshot = state
        .db
        .plugin_descriptor_authority_snapshot()
        .await
        .unwrap();
    assert_eq!(snapshot.revisions.len(), 105);
    assert_eq!(snapshot.revisions[0].inventory_id, "oldest");
    assert_eq!(snapshot.head.as_ref().unwrap().revision, 105);
    let response = export(&state, Some("newest"), 105).await;
    assert_eq!(
        response["descriptor"]["inventories"]
            .as_object()
            .unwrap()
            .len(),
        2
    );
    assert!(
        response["descriptor"]["inventories"]
            .get("oldest")
            .is_some()
    );
    assert_eq!(
        state
            .db
            .plugin_descriptor_authority_snapshot()
            .await
            .unwrap(),
        snapshot
    );
    let inventory_path = Path::new(state.config.plugin_inventory_file.as_ref().unwrap());
    let mut inventory: BTreeMap<String, PreinstalledInventory> =
        serde_json::from_slice(&std::fs::read(inventory_path).unwrap()).unwrap();
    inventory.remove("oldest");
    std::fs::write(inventory_path, serde_json::to_vec(&inventory).unwrap()).unwrap();
    let restarted = ApplicationPlugins::from_inventory_file(
        state.db.clone(),
        inventory_path.to_owned(),
        &PluginRuntime::default(),
    )
    .await
    .unwrap();
    assert!(matches!(
        restarted
            .export_descriptor(
                ExportPluginDescriptor {
                    inventory_id: Some("newest".into()),
                    expected_revision: 105
                },
                inventory_path.parent().unwrap(),
            )
            .await,
        Err(AppError::Forbidden)
    ));
}

#[tokio::test]
async fn descriptor_export_checks_global_write_authority_and_rejects_caller_roots() {
    let (_directory, state) = fixture().await;
    let input = json!({"expected_revision":0});
    assert_eq!(
        call(&state, "invalid", input.clone()).await.0,
        StatusCode::UNAUTHORIZED
    );
    state.db.create_tenant("export-tenant", None).await.unwrap();
    for tenant in [None, Some("export-tenant".to_owned())] {
        let token = state
            .db
            .create_service_token(
                crate::db::CreateServiceTokenInput {
                    name: format!("export-{}", tenant.as_deref().unwrap_or("global")),
                    scopes: if tenant.is_some() {
                        vec!["plugins:write".into()]
                    } else {
                        vec!["plugins:read".into()]
                    },
                    tenant_external_id: tenant,
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        assert_eq!(
            call(&state, &token.token, input.clone()).await.0,
            StatusCode::FORBIDDEN
        );
    }
    let token = state
        .db
        .create_service_token(
            crate::db::CreateServiceTokenInput {
                name: "export-authorized".into(),
                scopes: vec!["plugins:write".into()],
                tenant_external_id: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    assert_eq!(
        call(&state, &token.token, input.clone()).await.0,
        StatusCode::OK
    );
    for field in [
        "root",
        "destination_root",
        "grants",
        "descriptor",
        "approved",
        "readiness",
    ] {
        let mut input = input.clone();
        input[field] = json!("caller-supplied");
        assert!(
            call(&state, &state.config.service_token, input)
                .await
                .0
                .is_client_error()
        );
    }
    assert_eq!(
        call(
            &state,
            &state.config.service_token,
            json!({"inventory_id":"missing","expected_revision":0})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (_directory, mut disabled) = fixture().await;
    disabled.application_plugins = None;
    assert_eq!(
        call(
            &disabled,
            &disabled.config.service_token,
            json!({"expected_revision":0})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert!(disabled.pin_application_plugins().await.is_ok());
    assert!(
        state
            .db
            .plugin_descriptor_authority_snapshot()
            .await
            .unwrap()
            .head
            .is_none()
    );
}

async fn head_changes_during_export(state: &AppState, destination: &Path) {
    register_empty(state, "empty").await;
    let authority = state.application_plugins.as_ref().unwrap().clone();
    let (entered, entering) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    *authority.compile_gate.lock().unwrap() = Some((entered, released));
    let destination = destination.to_owned();
    let exporting = tokio::spawn(async move {
        authority
            .export_descriptor(
                ExportPluginDescriptor {
                    inventory_id: Some("empty".into()),
                    expected_revision: 0,
                },
                &destination,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), entering)
        .await
        .unwrap()
        .unwrap();
    let publication = tokio::time::timeout(
        Duration::from_secs(2),
        state.db.publish_application_plugin(
            "empty",
            0,
            "initial",
            "during-export",
            "during-export",
            "bootstrap",
        ),
    )
    .await;
    release.send(()).unwrap();
    publication
        .expect("asset validation must not retain a DB transaction")
        .unwrap();
    assert!(matches!(
        exporting.await.unwrap(),
        Err(AppError::Conflict(_))
    ));
    let (status, _) = call(
        state,
        &state.config.service_token,
        json!({"expected_revision":0}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    export(state, Some("empty"), 1).await;
}

#[tokio::test]
async fn descriptor_export_rechecks_expected_head_without_holding_a_write_lock() {
    let (directory, state) = fixture().await;
    head_changes_during_export(&state, directory.path()).await;
}

#[tokio::test]
async fn descriptor_export_postgres_snapshot_and_concurrent_head_change() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    sqlx::any::install_default_drivers();
    let pool = sqlx::AnyPool::connect(&url).await.unwrap();
    let schema = format!("descriptor_export_{}", uuid::Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&pool)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-c search_path={schema}"));
    let directory = tempfile::tempdir().unwrap();
    let inventory = directory.path().join("inventory.json");
    std::fs::write(&inventory, b"{}").unwrap();
    let mut config = Config::for_test(isolated.to_string());
    config.plugin_inventory_file = Some(inventory.to_str().unwrap().into());
    let state = AppState::initialize(config).await.unwrap();
    let absent = state
        .db
        .plugin_descriptor_authority_snapshot()
        .await
        .unwrap();
    assert!(absent.head.is_none() && absent.revisions.is_empty() && absent.candidates.is_empty());
    head_changes_during_export(&state, directory.path()).await;
    state.db.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}
