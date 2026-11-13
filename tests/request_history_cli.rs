use axum::{Router, body::Bytes, routing::post};
use memeloop_token_center::{
    archive::ArchiveStore,
    config::{ArchiveBackend, Config},
    db::{CreateKeyInput, Database, NewRequest},
    model::KeyPolicy,
};
use rust_decimal::Decimal;
use std::{
    process::Command,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_history_cli_loads_configured_archive_before_export() {
    let root = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        root.path().join("history.db").display()
    );
    let archive_path = root.path().join("archive");
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    database.history_start().await.unwrap();
    let pepper = b"test-pepper-must-have-at-least-32-bytes";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "cli-history".into(),
                principal_external_id: "member".into(),
                alias: "cli".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ZERO,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let key = database
        .authenticate_key(&issued.key, pepper)
        .await
        .unwrap();
    let mut config = Config::for_test(database_url.clone());
    config.archive_backend = ArchiveBackend::Filesystem;
    config.archive_path = Some(archive_path.to_string_lossy().into_owned());
    let archive = ArchiveStore::from_config(&config).await.unwrap();
    let request_id = Uuid::now_v7();
    let object = archive
        .put_tenant_content(key.tenant_id, Bytes::from_static(b"fixture"))
        .await
        .unwrap();
    archive
        .publish_raw_capture(key.tenant_id, request_id, "request", &object)
        .await
        .unwrap();
    database
        .record_request_started(NewRequest {
            request_id,
            key_id: key.key_id,
            tenant_id: key.tenant_id,
            protocol: "openai".into(),
            model: "history-model".into(),
            request_object: object.object_locator.clone(),
            reservation_id: Uuid::now_v7(),
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    drop(database);
    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured = bodies.clone();
    let router = Router::new().route(
        "/",
        post(move |body: Bytes| {
            let captured = captured.clone();
            async move {
                captured
                    .lock()
                    .unwrap()
                    .push(String::from_utf8(body.to_vec()).unwrap());
                ""
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_memeloop-token-center"))
            .args(["request-history", "backfill", "--page-size", "1"])
            .env(
                "MTC_KEY_PEPPER",
                String::from_utf8(pepper.to_vec()).unwrap(),
            )
            .env("MTC_SERVICE_TOKEN", "s".repeat(32))
            .env("MTC_DATABASE_URL", database_url)
            .env("MTC_ARCHIVE_BACKEND", "filesystem")
            .env("MTC_ARCHIVE_PATH", archive_path)
            .env("MTC_REQUEST_HISTORY_URL", endpoint)
            .env("MTC_REQUEST_HISTORY_DATABASE", "default")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let snapshots: Vec<serde_json::Value> = bodies
        .lock()
        .unwrap()
        .iter()
        .flat_map(|body| body.lines())
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|row| {
            row["snapshot"]
                .as_str()
                .map(|snapshot| serde_json::from_str(snapshot).unwrap())
        })
        .collect();
    let snapshot = snapshots
        .iter()
        .find(|snapshot| snapshot["identity"]["request_id"] == request_id.to_string())
        .expect("actual CLI must export the seeded native request");
    assert!(
        snapshot["read_model"]
            .to_string()
            .contains(&object.object_locator)
    );
    server.abort();
}
