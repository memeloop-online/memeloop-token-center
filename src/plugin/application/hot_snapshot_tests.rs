use super::*;

fn work_counts(authority: &ApplicationPlugins) -> [usize; 3] {
    let ordering = std::sync::atomic::Ordering::Relaxed;
    [
        authority.inventory_refreshes.load(ordering),
        authority.pin_root_inspections.load(ordering),
        authority.compilations.load(ordering),
    ]
}

async fn authority(directory: &std::path::Path) -> Arc<ApplicationPlugins> {
    let db = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.join("snapshot.db").display()
    ))
    .await
    .unwrap();
    db.migrate().await.unwrap();
    let runtime = PluginRuntime::load(None, db.clone()).unwrap();
    let inventories = ["a", "b"]
        .into_iter()
        .map(|id| {
            let root = directory.join(id);
            std::fs::create_dir(&root).unwrap();
            (
                id.to_owned(),
                PreinstalledInventory {
                    root,
                    grants: BTreeMap::new(),
                },
            )
        })
        .collect();
    Arc::new(ApplicationPlugins::new(db, inventories, &runtime).unwrap())
}

async fn publish(authority: &ApplicationPlugins, id: &str, expected: i64) {
    authority
        .publish(
            PublishApplicationPlugin {
                inventory_id: id.into(),
                expected_revision: expected,
            },
            id,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn hot_snapshot_survives_unavailable_database_inventory_and_load_locks_until_expiry() {
    let directory = tempfile::tempdir().unwrap();
    let authority = authority(directory.path()).await;
    assert!(authority.current_snapshot().is_err());
    assert_eq!(authority.refresh_status()["ready"], false);
    publish(&authority, "a", 0).await;
    authority.refresh_current().await.unwrap();
    let pinned = authority.current_snapshot().unwrap().unwrap();
    let work = work_counts(&authority);
    authority.db.close().await;
    std::fs::remove_dir_all(directory.path().join("a")).unwrap();
    let _inventory = authority.inventory_stamp.lock().await;
    let _cache = authority.snapshots.lock().await;
    for _ in 0..64 {
        assert!(Arc::ptr_eq(
            &pinned,
            &authority.current_snapshot().unwrap().unwrap()
        ));
    }
    assert_eq!(work_counts(&authority), work);
    assert!(authority.refresh_current().await.is_err());
    assert_eq!(
        authority.refresh_status()["error"],
        "snapshot_refresh_failed"
    );
    assert!(Arc::ptr_eq(
        &pinned,
        &authority.current_snapshot().unwrap().unwrap()
    ));
    authority.current.write().unwrap().confirmed = Some(tokio::time::Instant::now() - PIN_DEADLINE);
    assert!(matches!(
        authority.current_snapshot(),
        Err(AppError::Overloaded)
    ));
    assert_eq!(authority.refresh_status()["ready"], false);
    assert_eq!(pinned.receipt.revision, 1);
}

#[tokio::test]
async fn observed_new_head_withdraws_old_authority_while_compiling_then_swaps_whole_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let authority = authority(directory.path()).await;
    publish(&authority, "a", 0).await;
    authority.refresh_current().await.unwrap();
    let historical = authority.current_snapshot().unwrap().unwrap();
    publish(&authority, "b", 1).await;
    let (entered, entering) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    *authority.compile_gate.lock().unwrap() = Some((entered, released));
    let refreshing = authority.clone();
    let task = tokio::spawn(async move { refreshing.refresh_current().await });
    entering.await.unwrap();
    assert!(authority.current_snapshot().is_err());
    assert_eq!(authority.refresh_status()["revision"]["revision"], 1);
    assert_eq!(historical.receipt.revision, 1);
    release.send(()).unwrap();
    task.await.unwrap().unwrap();
    let current = authority.current_snapshot().unwrap().unwrap();
    assert_eq!(current.receipt.revision, 2);
    assert_eq!(current.runtime.receipt.revision, 2);
    assert!(!Arc::ptr_eq(&historical, &current));
    assert!(Arc::ptr_eq(
        &historical,
        &authority.pin_historical(1).await.unwrap()
    ));
}

#[tokio::test]
async fn missing_head_never_reverts_a_published_runtime_to_baseline_and_recovery_is_visible() {
    let directory = tempfile::tempdir().unwrap();
    let authority = authority(directory.path()).await;
    authority.refresh_current().await.unwrap();
    assert!(authority.current_snapshot().unwrap().is_none());
    publish(&authority, "a", 0).await;
    authority.refresh_current().await.unwrap();
    let pinned = authority.current_snapshot().unwrap().unwrap();
    let pool = sqlx::AnyPool::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("snapshot.db").display()
    ))
    .await
    .unwrap();
    sqlx::query("DELETE FROM application_plugin_head WHERE scope = 'global'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(authority.refresh_current().await.is_err());
    assert!(authority.current_snapshot().is_err());
    assert_eq!(authority.refresh_status()["revision"]["revision"], 1);
    let restarted = Arc::new(
        ApplicationPlugins::new(
            authority.db.clone(),
            BTreeMap::new(),
            pinned.runtime.runtime(),
        )
        .unwrap(),
    );
    assert!(restarted.refresh_current().await.is_err());
    assert!(restarted.current_snapshot().is_err());
    sqlx::query("INSERT INTO application_plugin_head (scope, revision) VALUES ('global', 1)")
        .execute(&pool)
        .await
        .unwrap();
    authority.refresh_current().await.unwrap();
    assert_eq!(authority.refresh_status()["error"], serde_json::Value::Null);
    assert!(Arc::ptr_eq(
        &pinned,
        &authority.current_snapshot().unwrap().unwrap()
    ));
}

#[tokio::test]
async fn head_changed_during_compilation_is_not_activated() {
    let directory = tempfile::tempdir().unwrap();
    let authority = authority(directory.path()).await;
    publish(&authority, "a", 0).await;
    authority.refresh_current().await.unwrap();
    let original = authority.current_snapshot().unwrap().unwrap();
    publish(&authority, "b", 1).await;
    let (entered, entering) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    *authority.compile_gate.lock().unwrap() = Some((entered, released));
    let refreshing = authority.clone();
    let task = tokio::spawn(async move { refreshing.refresh_current().await });
    entering.await.unwrap();
    let pool = sqlx::AnyPool::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("snapshot.db").display()
    ))
    .await
    .unwrap();
    sqlx::query("UPDATE application_plugin_head SET revision = 1 WHERE scope = 'global'")
        .execute(&pool)
        .await
        .unwrap();
    release.send(()).unwrap();
    assert!(matches!(task.await.unwrap(), Err(AppError::Conflict(_))));
    assert!(authority.current_snapshot().is_err());
    authority.refresh_current().await.unwrap();
    assert!(Arc::ptr_eq(
        &original,
        &authority.current_snapshot().unwrap().unwrap()
    ));
}

#[tokio::test]
async fn cancelled_refresh_retains_verified_object_and_reports_failure_without_reauthorizing_it() {
    let directory = tempfile::tempdir().unwrap();
    let authority = authority(directory.path()).await;
    publish(&authority, "a", 0).await;
    authority.refresh_current().await.unwrap();
    let original = authority.current_snapshot().unwrap().unwrap();
    publish(&authority, "b", 1).await;
    let (entered, entering) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    *authority.compile_gate.lock().unwrap() = Some((entered, released));
    let refreshing = authority.clone();
    let task = tokio::spawn(async move { refreshing.refresh_current().await });
    entering.await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(authority.current_snapshot().is_err());
    assert_eq!(
        authority.refresh_status()["error"],
        "snapshot_refresh_cancelled"
    );
    assert!(Arc::ptr_eq(
        &original,
        authority.current.read().unwrap().snapshot.as_ref().unwrap()
    ));
    release.send(()).unwrap();
    authority.refresh_current().await.unwrap();
    assert_eq!(
        authority
            .current_snapshot()
            .unwrap()
            .unwrap()
            .receipt
            .revision,
        2
    );
    assert_eq!(authority.refresh_status()["error"], serde_json::Value::Null);
}

#[tokio::test]
async fn cold_snapshot_keeps_liveness_healthy_and_readiness_unavailable_until_background_warm() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let directory = tempfile::tempdir().unwrap();
    let authority = authority(directory.path()).await;
    publish(&authority, "a", 0).await;
    let config = crate::config::Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("snapshot.db").display()
    ));
    let mut state = crate::AppState::initialize(config).await.unwrap();
    state.application_plugins = Some(authority.clone());
    let app = crate::api::router_for_role(state.clone(), crate::config::RuntimeRole::Gateway);
    for (path, expected) in [
        ("/livez", StatusCode::OK),
        ("/readyz", StatusCode::SERVICE_UNAVAILABLE),
    ] {
        let response = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    assert!(state.clone().pin_application_plugins().await.is_err());
    state.refresh_application_plugins().await.unwrap();
    assert!(state.pin_application_plugins().await.is_ok());
    let response = app
        .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
