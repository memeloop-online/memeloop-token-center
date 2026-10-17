use super::*;
use crate::{AppState, config::Config};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use tower::ServiceExt;

async fn request(
    state: &AppState,
    body: serde_json::Value,
    key: Option<&str>,
    token: &str,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::post("/internal/v1/plugin-runtime/uninstall")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json");
    if let Some(key) = key {
        request = request.header("idempotency-key", key);
    }
    let response = crate::api::router_for_role(state.clone(), crate::config::RuntimeRole::Control)
        .oneshot(
            request
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn fixture() -> (tempfile::TempDir, Config, AppState) {
    fixture_with_alternatives(false).await
}

async fn fixture_with_alternatives(alternatives: bool) -> (tempfile::TempDir, Config, AppState) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("approved");
    let inventory_file = directory.path().join("inventory.json");
    for id in ["removed-plugin", "retained-plugin"] {
        let package = root.join(id);
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("plugin.json"),
            serde_json::to_vec(&json!({
                "id":id, "version":"1.0.0", "wit_version":"0.2.0", "wasm":null,
                "capabilities":[], "contributions":{},
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(package.join(".mtc-oci-install.json"), serde_json::to_vec(&json!({
            "format_version":1, "source":"ghcr.io/example/uninstall-test",
            "digest":format!("sha256:{}", "a".repeat(64)), "signature_policy":"cosign-public-key",
        })).unwrap()).unwrap();
        std::fs::write(package.join("asset.txt"), b"preserved asset").unwrap();
        std::fs::create_dir(package.join("ui")).unwrap();
        std::fs::write(package.join("ui/index.html"), b"retained UI").unwrap();
    }
    let mut config = Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("test.db").display()
    ));
    let base = AppState::initialize(config.clone()).await.unwrap();
    let runtime = PluginRuntime::load(root.to_str(), base.db.clone()).unwrap();
    let identities = runtime.package_identities();
    let mut grants: BTreeMap<String, Vec<PluginGrant>> = runtime
        .manifests()
        .into_iter()
        .map(|manifest| {
            (
                manifest.id.clone(),
                vec![PluginGrant {
                    version: manifest.version.clone(),
                    capabilities: manifest.capabilities.clone(),
                    manifest_digest: lifecycle::manifest_digest(&manifest).unwrap(),
                    identity: identities[&manifest.id].clone(),
                }],
            )
        })
        .collect();
    if alternatives {
        let mut manifest = runtime
            .manifests()
            .into_iter()
            .find(|manifest| manifest.id == "retained-plugin")
            .unwrap();
        manifest.version = "1.0.1".into();
        let mut alternative = grants["retained-plugin"][0].clone();
        alternative.version = manifest.version.clone();
        alternative.manifest_digest = lifecycle::manifest_digest(&manifest).unwrap();
        grants.get_mut("retained-plugin").unwrap().push(alternative);
    }
    let inventory = BTreeMap::from([(
        "approved".to_owned(),
        PreinstalledInventory { root, grants },
    )]);
    std::fs::write(&inventory_file, serde_json::to_vec(&inventory).unwrap()).unwrap();
    config.plugin_inventory_file = Some(inventory_file.to_str().unwrap().into());
    let state = AppState::initialize(config.clone()).await.unwrap();
    state
        .application_plugins
        .as_ref()
        .unwrap()
        .publish(
            PublishApplicationPlugin {
                inventory_id: "approved".into(),
                expected_revision: 0,
            },
            "initial",
        )
        .await
        .unwrap();
    (directory, config, state)
}

#[tokio::test]
async fn subset_removal_restart_last_broken_plugin_and_replay() {
    let (directory, config, state) = fixture().await;
    let body = json!({"plugin_id":"removed-plugin", "expected_revision":1});
    // Broken removed packages cannot be compiled, but removing them still works.
    std::fs::write(
        directory.path().join("approved/removed-plugin/plugin.json"),
        b"broken",
    )
    .unwrap();
    let (status, receipt) = request(
        &state,
        body.clone(),
        Some("remove-one"),
        &state.config.service_token,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(receipt["revision"], 2);
    let restarted = AppState::initialize(config.clone()).await.unwrap();
    let pinned = restarted.clone().pin_application_plugins().await.unwrap();
    assert_eq!(pinned.plugins.manifests().len(), 1);
    assert_eq!(pinned.plugins.manifests()[0].id, "retained-plugin");
    let inventory: BTreeMap<String, PreinstalledInventory> =
        serde_json::from_slice(&std::fs::read(directory.path().join("inventory.json")).unwrap())
            .unwrap();
    let subset = &inventory[receipt["inventory_id"].as_str().unwrap()];
    assert!(
        subset
            .root
            .join("retained-plugin/.mtc-oci-install.json")
            .is_file()
    );
    assert_eq!(
        std::fs::read(subset.root.join("retained-plugin/asset.txt")).unwrap(),
        b"preserved asset"
    );
    assert!(
        directory
            .path()
            .join("approved/removed-plugin/asset.txt")
            .is_file()
    );
    std::fs::remove_dir_all(&subset.root).unwrap();
    let (status, empty) = request(
        &restarted,
        json!({"plugin_id":"retained-plugin", "expected_revision":2}),
        Some("remove-last"),
        &restarted.config.service_token,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty["revision"], 3);
    let final_state = AppState::initialize(config).await.unwrap();
    assert!(
        final_state
            .clone()
            .pin_application_plugins()
            .await
            .unwrap()
            .plugins
            .manifests()
            .is_empty()
    );
    let (status, replay) = request(
        &final_state,
        body,
        Some("remove-one"),
        &final_state.config.service_token,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(receipt, replay);
    let audit = final_state.db.plugin_audit(None).await.unwrap();
    assert_eq!(
        audit
            .iter()
            .filter(|entry| entry.action == "uninstall")
            .count(),
        2
    );
}

#[tokio::test]
async fn management_boundary_unknown_plugin_stale_revision_and_idempotency_conflicts() {
    let (_directory, _config, state) = fixture().await;
    let token = &state.config.service_token;
    state.db.create_tenant("tenant", None).await.unwrap();
    for tenant in [None, Some("tenant".to_owned())] {
        let scoped = state
            .db
            .create_service_token(
                crate::db::CreateServiceTokenInput {
                    name: format!("uninstall-{}", tenant.as_deref().unwrap_or("reader")),
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
            request(
                &state,
                json!({"plugin_id":"removed-plugin", "expected_revision":1}),
                Some("scoped-denied"),
                &scoped.token
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    let body = json!({"plugin_id":"removed-plugin", "expected_revision":1});
    assert_eq!(
        request(&state, body.clone(), None, token).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&state, body.clone(), Some("bad-token"), "invalid")
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            &state,
            json!({"plugin_id":"absent", "expected_revision":1}),
            Some("absent"),
            token
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &state,
            json!({"plugin_id":"removed-plugin", "expected_revision":0}),
            Some("zero"),
            token
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(
            &state,
            json!({"plugin_id":"removed-plugin", "expected_revision":1, "grants":{}}),
            Some("forged"),
            token
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        request(&state, body, Some("remove"), token).await.0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &state,
            json!({"plugin_id":"retained-plugin", "expected_revision":2}),
            Some("remove"),
            token
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(
            &state,
            json!({"plugin_id":"retained-plugin", "expected_revision":1}),
            Some("stale"),
            token
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let authority = state.application_plugins.as_ref().unwrap();
    assert!(matches!(
        authority
            .uninstall(
                UninstallApplicationPlugin {
                    plugin_id: "removed-plugin".into(),
                    expected_revision: 1
                },
                "remove",
                "different-actor"
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    assert_eq!(
        authority.status().await.unwrap().current.unwrap().revision,
        2
    );
}

#[cfg(target_os = "linux")]
#[test]
fn subset_copy_rejects_links_and_preserves_hidden_files() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join(".receipt"), b"receipt").unwrap();
    let target = directory.path().join("copy");
    copy_package(std::fs::File::open(&source).unwrap(), &target, &mut 0).unwrap();
    assert_eq!(std::fs::read(target.join(".receipt")).unwrap(), b"receipt");
    symlink("/etc/passwd", source.join("escape")).unwrap();
    assert!(
        copy_package(
            std::fs::File::open(source).unwrap(),
            &directory.path().join("rejected"),
            &mut 0
        )
        .is_err()
    );
}

#[tokio::test]
async fn concurrent_removals_never_rebase_onto_each_other() {
    let (_directory, _config, state) = fixture().await;
    let authority = state.application_plugins.as_ref().unwrap();
    let (first, second) = tokio::join!(
        authority.uninstall(
            UninstallApplicationPlugin {
                plugin_id: "removed-plugin".into(),
                expected_revision: 1
            },
            "race-one",
            "bootstrap"
        ),
        authority.uninstall(
            UninstallApplicationPlugin {
                plugin_id: "retained-plugin".into(),
                expected_revision: 1
            },
            "race-two",
            "bootstrap"
        ),
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    let loser = if first.is_ok() { second } else { first };
    assert!(matches!(loser, Err(AppError::Conflict(_))));
    assert_eq!(
        authority.status().await.unwrap().current.unwrap().revision,
        2
    );
    assert_eq!(
        state
            .clone()
            .pin_application_plugins()
            .await
            .unwrap()
            .plugins
            .manifests()
            .len(),
        1
    );
}

#[tokio::test]
async fn removal_cannot_silently_choose_another_approved_retained_version() {
    let (directory, _config, state) = fixture_with_alternatives(true).await;
    let path = directory
        .path()
        .join("approved/retained-plugin/plugin.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    manifest["version"] = json!("1.0.1");
    std::fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let status = request(
        &state,
        json!({"plugin_id":"removed-plugin", "expected_revision":1}),
        Some("cannot-upgrade-retained"),
        &state.config.service_token,
    )
    .await
    .0;
    assert_eq!(status, StatusCode::CONFLICT);
    let status = state
        .application_plugins
        .as_ref()
        .unwrap()
        .status()
        .await
        .unwrap();
    assert_eq!(status.current.unwrap().revision, 1);
    assert_eq!(status.candidates.len(), 1);
}
