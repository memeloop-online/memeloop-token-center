use super::*;
use crate::{AppState, config::Config};
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use tower::ServiceExt;
use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
use wit_parser::Resolve;

const PROVIDER: &str = "example-oauth-http";

fn write_inventory(root: &std::path::Path, second: bool) {
    let package = root.join("example-policy-rewrite");
    std::fs::create_dir_all(&package).unwrap();
    let mut manifest: serde_json::Value = serde_json::from_str(include_str!(
        "../../../examples/plugins/policy-rewrite/plugin.json"
    ))
    .unwrap();
    if second {
        manifest["version"] = json!("1.0.1");
    }
    std::fs::write(
        package.join("plugin.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let mut source = include_str!("../../../examples/plugins/policy-rewrite/plugin.wat").to_owned();
    if second {
        // Equal-length replacements preserve the fixture's canonical ABI offsets.
        source = source
            .replace("example-rewritten", "example-revisionb")
            .replace("buffered-v1", "buffered-v2")
            .replace("input_tokens\\22:7", "input_tokens\\22:8");
    }
    let mut module = wat::parse_str(source).unwrap();
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
    // Test-only host-approved provenance. No production signature verifier is
    // bypassed: the runtime compares this fixture to independent test grants.
    std::fs::write(
        package.join(".mtc-oci-install.json"),
        serde_json::to_vec(&json!({
            "format_version": 1, "source": "ghcr.io/example/test-inventory",
            "digest": format!("sha256:{}", if second { "b" } else { "a" }.repeat(64)),
            "signature_policy": "cosign-public-key"
        }))
        .unwrap(),
    )
    .unwrap();
}

fn inventory(db: &Database, roots: &[(&str, PathBuf)]) -> BTreeMap<String, PreinstalledInventory> {
    roots
        .iter()
        .map(|(id, root)| {
            let runtime = PluginRuntime::load(root.to_str(), db.clone()).unwrap();
            let identities = runtime.package_identities();
            let grants = runtime
                .manifests()
                .into_iter()
                .map(|manifest| {
                    let grant = PluginGrant {
                        version: manifest.version.clone(),
                        capabilities: manifest.capabilities.clone(),
                        manifest_digest: lifecycle::manifest_digest(&manifest).unwrap(),
                        identity: identities[&manifest.id].clone(),
                    };
                    (manifest.id, vec![grant])
                })
                .collect();
            (
                (*id).to_owned(),
                PreinstalledInventory {
                    root: root.clone(),
                    grants,
                },
            )
        })
        .collect()
}

fn context() -> super::super::types::RequestContext {
    super::super::types::RequestContext {
        tenant_id: "tenant".into(),
        principal_id: "principal".into(),
        key_id: "key".into(),
        protocol: "openai".into(),
        model: "model".into(),
        config_json: "{}".into(),
    }
}

async fn assert_policy(state: &AppState, second: bool) {
    let snapshot = state.pinned_application_plugins.clone();
    let baseline = state.plugins.clone();
    let expected = if second {
        "example-revisionb"
    } else {
        "example-rewritten"
    };
    let decision = tokio::task::spawn_blocking(move || match snapshot {
        Some(snapshot) => snapshot.runtime.apply_traffic_with_config(
            context(),
            &json!({"model":"model"}),
            &BTreeMap::new(),
        ),
        None => baseline.apply_traffic_with_config(
            context(),
            &json!({"model":"model"}),
            &BTreeMap::new(),
        ),
    })
    .await
    .unwrap()
    .unwrap();
    assert!(decision.allow);
    assert_eq!(decision.model.as_deref(), Some(expected));
}

async fn assert_provider_phases(state: &AppState, second: bool) {
    let plugins = state.plugins.clone();
    let (prepared, normalized) = tokio::task::spawn_blocking(move || {
        let prepared = plugins
            .prepare_provider_request(PROVIDER, context(), &json!({}), &json!({"model":"model"}))
            .unwrap()
            .unwrap();
        let normalized = plugins
            .normalize_provider_response(PROVIDER, context(), 200, &BTreeMap::new(), b"{}")
            .unwrap()
            .unwrap();
        (prepared, normalized)
    })
    .await
    .unwrap();
    assert_eq!(
        prepared.headers["x-plugin-shape"],
        if second { "buffered-v2" } else { "buffered-v1" }
    );
    assert_eq!(normalized.input_tokens, if second { 8 } else { 7 });
    assert_eq!(
        state.providers.get(PROVIDER).unwrap().source,
        format!(
            "plugin:example-policy-rewrite@{}",
            if second { "1.0.1" } else { "1.0.0" }
        )
    );
}

fn publish(id: &str, expected: i64) -> PublishApplicationPlugin {
    PublishApplicationPlugin {
        inventory_id: id.into(),
        expected_revision: expected,
    }
}

async fn exercise_authority(database_url: String, directory: &std::path::Path, concurrent: bool) {
    let a_root = directory.join("inventory-a");
    let b_root = directory.join("inventory-b");
    write_inventory(&a_root, false);
    write_inventory(&b_root, true);
    let mut config = Config::for_test(database_url.clone());
    config.plugin_dir = a_root.to_str().map(str::to_owned);
    let first = AppState::initialize(config.clone()).await.unwrap();
    let trusted = inventory(&first.db, &[("a", a_root.clone()), ("b", b_root.clone())]);
    let inventory_path = directory.join("host-inventory.json");
    std::fs::write(&inventory_path, serde_json::to_vec(&trusted).unwrap()).unwrap();
    config.plugin_inventory_file = Some(inventory_path.to_str().unwrap().to_owned());
    let first = AppState::initialize(config.clone()).await.unwrap();
    let second = AppState::initialize(config.clone()).await.unwrap();
    let authority_a = first.application_plugins.clone().unwrap();
    let authority_b = second.application_plugins.clone().unwrap();
    let baseline = first.clone().pin_application_plugins().await.unwrap();
    assert_policy(&baseline, false).await;
    let status = control_get(&first, "/internal/v1/plugin-runtime").await;
    assert!(status["current"].is_null());
    assert_eq!(status["candidates"].as_array().unwrap().len(), 2);
    assert!(!status.to_string().contains(directory.to_str().unwrap()));
    assert_eq!(
        management_call(
            &first,
            &first.config.service_token,
            json!({"inventory_id":"a", "expected_revision":0})
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        control_get(&second, "/internal/v1/plugin-runtime").await["current"]["revision"],
        1
    );
    first.db.migrate().await.unwrap();
    first.db.migrate().await.unwrap();

    // Cold concurrent pins must converge to the same runtime/catalog/circuit
    // object, not merely equivalent freshly compiled executables.
    let before = authority_a
        .compilations
        .load(std::sync::atomic::Ordering::Relaxed);
    let (entered, entering) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    *authority_a.compile_gate.lock().unwrap() = Some((entered, released));
    let leader_authority = authority_a.clone();
    let leader = tokio::spawn(async move { leader_authority.pin().await });
    entering.await.unwrap();
    leader.abort();
    assert!(matches!(leader.await, Err(error) if error.is_cancelled()));
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut pins = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let authority = authority_a.clone();
        let barrier = barrier.clone();
        pins.spawn(async move {
            barrier.wait().await;
            authority.pin().await.unwrap()
        });
    }
    release.send(()).unwrap();
    let initial = pins.join_next().await.unwrap().unwrap();
    while let Some(pin) = pins.join_next().await {
        assert!(Arc::ptr_eq(&initial, &pin.unwrap()));
    }
    assert_eq!(
        authority_a
            .compilations
            .load(std::sync::atomic::Ordering::Relaxed),
        before + 1
    );

    // A matching revision number is not sufficient authority for a warm hit.
    // Simulate a corrupt/mismatched primary receipt without touching the package.
    let inspection_pool = sqlx::AnyPool::connect(&database_url).await.unwrap();
    sqlx::query("UPDATE application_plugin_candidates SET identity_digest = 'mismatched' WHERE inventory_id = 'a'")
        .execute(&inspection_pool).await.unwrap();
    assert!(authority_a.pin().await.is_err());
    sqlx::query(
        "UPDATE application_plugin_candidates SET identity_digest = $1 WHERE inventory_id = 'a'",
    )
    .bind(&initial.receipt.identity_digest)
    .execute(&inspection_pool)
    .await
    .unwrap();
    assert!(Arc::ptr_eq(&initial, &authority_a.pin().await.unwrap()));
    inspection_pool.close().await;

    // A request is blocked after policy; B publishes while it is parked. No
    // sleeps, time thresholds, or notification delivery are involved.
    let (policy_done, wait_policy) = tokio::sync::oneshot::channel();
    let (resume, wait_resume) = tokio::sync::oneshot::channel();
    let request_state = first.clone();
    let request = tokio::spawn(async move {
        let pinned = request_state.pin_application_plugins().await.unwrap();
        assert_policy(&pinned, false).await;
        policy_done.send(()).unwrap();
        wait_resume.await.unwrap();
        let pinned = pinned.pin_application_plugins().await.unwrap();
        assert_eq!(
            pinned
                .pinned_application_plugins
                .as_ref()
                .unwrap()
                .receipt
                .revision,
            1
        );
        assert_provider_phases(&pinned, false).await;
    });
    wait_policy.await.unwrap();
    assert_eq!(
        authority_b
            .publish(publish("b", 1), "switch-b")
            .await
            .unwrap()
            .revision,
        2
    );
    let fresh = first.clone().pin_application_plugins().await.unwrap();
    assert_policy(&fresh, true).await;
    assert_provider_phases(&fresh, true).await;
    assert_control_revision(&second, "1.0.1", 2).await;
    // Even a request begun before first publication is an immutable baseline pin.
    assert_policy(&baseline.pin_application_plugins().await.unwrap(), false).await;
    resume.send(()).unwrap();
    request.await.unwrap();

    // Exact replay returns its original receipt, even after another head won.
    assert_eq!(
        authority_a
            .publish(publish("a", 0), "management-test")
            .await
            .unwrap()
            .revision,
        1
    );
    assert!(matches!(
        authority_a
            .publish(publish("b", 1), "management-test")
            .await,
        Err(AppError::Conflict(_))
    ));
    assert!(matches!(
        authority_a.publish(publish("a", 1), "stale").await,
        Err(AppError::Conflict(_))
    ));

    let restarted = AppState::initialize(config).await.unwrap();
    assert_provider_phases(
        &restarted.clone().pin_application_plugins().await.unwrap(),
        true,
    )
    .await;
    let rolled = authority_a
        .rollback(
            RollbackApplicationPlugin {
                target_revision: 1,
                expected_revision: 2,
            },
            "rollback",
        )
        .await
        .unwrap();
    assert_eq!(rolled.revision, 3);
    assert_eq!(rolled.inventory_id, "a");
    assert_control_revision(&second, "1.0.0", 3).await;
    assert_provider_phases(
        &second.clone().pin_application_plugins().await.unwrap(),
        false,
    )
    .await;
    assert_eq!(
        authority_b
            .rollback(
                RollbackApplicationPlugin {
                    target_revision: 1,
                    expected_revision: 2
                },
                "rollback"
            )
            .await
            .unwrap()
            .revision,
        3
    );

    if concurrent {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let left = authority_a.clone();
        let gate = barrier.clone();
        let one = tokio::spawn(async move {
            gate.wait().await;
            left.publish(publish("a", 3), "cas-a").await
        });
        let right = authority_b.clone();
        let two = tokio::spawn(async move {
            barrier.wait().await;
            right.publish(publish("b", 3), "cas-b").await
        });
        let results = [one.await.unwrap(), two.await.unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(AppError::Conflict(_))))
                .count(),
            1
        );
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let left = authority_a.clone();
        let gate = barrier.clone();
        let one = tokio::spawn(async move {
            gate.wait().await;
            left.publish(publish("a", 4), "concurrent-replay").await
        });
        let right = authority_b.clone();
        let two = tokio::spawn(async move {
            barrier.wait().await;
            right.publish(publish("a", 4), "concurrent-replay").await
        });
        assert_eq!(one.await.unwrap().unwrap().revision, 5);
        assert_eq!(two.await.unwrap().unwrap().revision, 5);
    }

    // Bound manager-owned history while an old request remains independently
    // pinned. Each new revision still follows the current database authority.
    for index in 0..4 {
        let current = first.db.application_plugin_head().await.unwrap();
        authority_a
            .publish(
                publish("a", current.revision),
                &format!("cache-bound-{index}"),
            )
            .await
            .unwrap();
        let newest = authority_a.pin().await.unwrap();
        assert_eq!(newest.receipt.revision, current.revision + 1);
        assert!(authority_a.snapshots.lock().await.snapshots.len() <= CACHED_REVISIONS);
    }
    assert_eq!(initial.receipt.revision, 1);
    assert_eq!(
        authority_a.snapshots.lock().await.snapshots.len(),
        CACHED_REVISIONS
    );

    // Tampering cannot change a persisted inventory identity, and an absent
    // local package cannot fall back to the old AppState startup runtime.
    let manifest = a_root.join("example-policy-rewrite/plugin.json");
    let original = std::fs::read(&manifest).unwrap();
    let mut changed: serde_json::Value = serde_json::from_slice(&original).unwrap();
    changed["contributions"]["providers"][0]["display_name"] = json!("forged contract");
    std::fs::write(&manifest, serde_json::to_vec(&changed).unwrap()).unwrap();
    assert!(authority_a.stage("a").await.is_err());
    std::fs::write(&manifest, original).unwrap();
    std::fs::rename(&a_root, directory.join("removed-inventory-a")).unwrap();
    assert!(first.clone().pin_application_plugins().await.is_err());
    for path in [
        "/internal/v1/plugins",
        "/internal/v1/provider-types",
        "/internal/v1/plugins/example-policy-rewrite/configuration",
        "/internal/v1/plugins/example-policy-rewrite/data/feed",
    ] {
        let response =
            crate::api::router_for_role(second.clone(), crate::config::RuntimeRole::Control)
                .oneshot(
                    Request::get(path)
                        .header(
                            header::AUTHORIZATION,
                            format!("Bearer {}", second.config.service_token),
                        )
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "{path} must not use baseline"
        );
    }
    std::fs::rename(directory.join("removed-inventory-a"), &a_root).unwrap();
    let pinned = first.clone().pin_application_plugins().await.unwrap();
    first.db.close().await;
    assert!(first.pin_application_plugins().await.is_err());
    assert_provider_phases(&pinned, false).await;
}

#[tokio::test]
async fn sqlite_application_snapshot_restart_rollback_and_migration_replay() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("authority.db").display()
    );
    exercise_authority(url, directory.path(), false).await;
}

#[tokio::test]
async fn postgres_two_appstates_cas_idempotency_missed_notifications_and_replay() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    sqlx::any::install_default_drivers();
    let pool = sqlx::AnyPool::connect(&url).await.unwrap();
    // UUID is namespace isolation only, never a concurrency scheduling input.
    let schema = format!("plugin_revision_test_{}", uuid::Uuid::now_v7().simple());
    // SQL identifier consists solely of the fixed prefix and UUID hex digits.
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&pool)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-c search_path={schema}"));
    let directory = tempfile::tempdir().unwrap();
    exercise_authority(isolated.to_string(), directory.path(), true).await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

async fn management_call(state: &AppState, token: &str, body: serde_json::Value) -> StatusCode {
    crate::api::router_for_role(state.clone(), crate::config::RuntimeRole::Control)
        .oneshot(
            Request::post("/internal/v1/plugin-runtime/publish")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("idempotency-key", "management-test")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn control_get(state: &AppState, path: &str) -> serde_json::Value {
    let response = crate::api::router_for_role(state.clone(), crate::config::RuntimeRole::Control)
        .oneshot(
            Request::get(path)
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", state.config.service_token),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{path}");
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

async fn assert_control_revision(state: &AppState, version: &str, revision: i64) {
    let manifests = control_get(state, "/internal/v1/plugins").await;
    assert_eq!(manifests[0]["version"], version);
    let providers = control_get(state, "/internal/v1/provider-types").await;
    let pinned = state.clone().pin_application_plugins().await.unwrap();
    assert_eq!(
        providers,
        serde_json::to_value(pinned.providers.list()).unwrap()
    );
    let status = control_get(state, "/internal/v1/plugin-runtime/candidates").await;
    assert_eq!(status["current"]["revision"], revision);
    assert!(
        status["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .all(|candidate| candidate["staged"] == true)
    );
}

#[tokio::test]
async fn configured_empty_inventory_preserves_baseline_and_reports_no_revision() {
    let directory = tempfile::tempdir().unwrap();
    let inventory_path = directory.path().join("empty.json");
    std::fs::write(&inventory_path, b"{}").unwrap();
    let mut config = Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("empty.db").display()
    ));
    config.plugin_inventory_file = Some(inventory_path.to_str().unwrap().to_owned());
    let state = AppState::initialize(config.clone()).await.unwrap();
    assert!(
        state
            .clone()
            .pin_application_plugins()
            .await
            .unwrap()
            .plugins
            .manifests()
            .is_empty()
    );
    assert_eq!(
        control_get(&state, "/internal/v1/plugin-runtime").await,
        json!({"current":null,"candidates":[]})
    );
    std::fs::write(&inventory_path, b"{invalid").unwrap();
    assert!(matches!(
        AppState::initialize(config).await,
        Err(crate::InitializationError::Plugin)
    ));
}

#[tokio::test]
async fn runtime_management_rejects_scoped_credentials_and_forged_candidate_fields() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("management.db").display()
    );
    let state = AppState::initialize(Config::for_test(url))
        .await
        .unwrap()
        .with_application_plugin_inventory(BTreeMap::new())
        .unwrap();
    let body = json!({"inventory_id":"a", "expected_revision":0});
    state.db.create_tenant("tenant", None).await.unwrap();
    for tenant in [None, Some("tenant".to_owned())] {
        let tenant_scoped = tenant.is_some();
        let scoped = state
            .db
            .create_service_token(
                crate::db::CreateServiceTokenInput {
                    name: format!(
                        "runtime-management-{}",
                        tenant.as_deref().unwrap_or("global")
                    ),
                    scopes: if tenant.is_some() {
                        vec!["plugins:write".into(), "plugins:read".into()]
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
            management_call(&state, &scoped.token, body.clone()).await,
            StatusCode::FORBIDDEN
        );
        let response =
            crate::api::router_for_role(state.clone(), crate::config::RuntimeRole::Control)
                .oneshot(
                    Request::get("/internal/v1/plugin-runtime")
                        .header(header::AUTHORIZATION, format!("Bearer {}", scoped.token))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
        assert_eq!(
            response.status(),
            if tenant_scoped {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::OK
            }
        );
    }
    for field in ["url", "path", "wasm", "grant", "tenant_external_id"] {
        let mut forged = body.clone();
        forged[field] = json!("forged");
        assert_eq!(
            management_call(&state, &state.config.service_token, forged).await,
            StatusCode::BAD_REQUEST
        );
    }
    for id in ["../a", "https://example.com/a", "/tmp/a", "a.wasm"] {
        assert_eq!(
            management_call(
                &state,
                &state.config.service_token,
                json!({"inventory_id":id,"expected_revision":0})
            )
            .await,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        management_call(&state, &state.config.service_token, body).await,
        StatusCode::FORBIDDEN
    );
}
