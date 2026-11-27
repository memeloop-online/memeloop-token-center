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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_history_cli_backfill_page_queues_without_archive_or_delivery() {
    let root = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        root.path().join("page.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let pepper = b"test-pepper-must-have-at-least-32-bytes";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "cli-page".into(),
                principal_external_id: "member".into(),
                alias: "page".into(),
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
    let request = Uuid::now_v7();
    database
        .record_request_started(NewRequest {
            request_id: request,
            key_id: key.key_id,
            tenant_id: key.tenant_id,
            protocol: "openai".into(),
            model: "history-model".into(),
            request_object: "gap://fixture/request".into(),
            reservation_id: Uuid::now_v7(),
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    database.history_start().await.unwrap();
    let pool = sqlx::AnyPool::connect(&database_url).await.unwrap();
    // Isolate one native source page. The CLI must commit its journal entry
    // without trying to materialize or ACK this retained source.
    sqlx::query("UPDATE request_history_backfill SET done=1 WHERE source<>'native'")
        .execute(&pool)
        .await
        .unwrap();
    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_history_export")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(before, 0, "the request was created while capture was off");
    let unused_archive = root.path().join("must-not-initialize");
    for expected_queued in [1, 0] {
        let url = database_url.clone();
        let archive_path = unused_archive.clone();
        let output = tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_memeloop-token-center"))
                .args(["request-history", "backfill-page"])
                .env(
                    "MTC_KEY_PEPPER",
                    String::from_utf8(pepper.to_vec()).unwrap(),
                )
                .env("MTC_SERVICE_TOKEN", "s".repeat(32))
                .env("MTC_DATABASE_URL", url)
                .env("MTC_ARCHIVE_BACKEND", "filesystem")
                .env("MTC_ARCHIVE_PATH", archive_path)
                .env_remove("MTC_REQUEST_HISTORY_URL")
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
        let receipt: serde_json::Value = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|value| value.get("queued").is_some())
            .unwrap();
        assert_eq!(receipt["queued"], expected_queued);
        assert_eq!(receipt["backfill_done"], true);
        assert!(
            !unused_archive.exists(),
            "metadata queueing must not initialize an archive"
        );
        let retained: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM request_history_export WHERE request_id=$1 AND snapshot IS NULL",
        )
        .bind(request.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            retained, 1,
            "queue-only CLI must never ACK the retained request"
        );
    }
    pool.close().await;
    drop(database);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_history_cli_export_page_reports_safe_materialization_failure_without_ack() {
    let root = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        root.path().join("export-page.db").display()
    );
    let database = Database::connect(&url).await.unwrap();
    database.migrate().await.unwrap();
    let pool = sqlx::AnyPool::connect(&url).await.unwrap();
    sqlx::query("UPDATE request_history_state SET mode='shadow' WHERE id=1")
        .execute(&pool)
        .await
        .unwrap();
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO request_history_immutable_outbox (revision,source,request_id,tenant_id,key_id,created_at,snapshot) VALUES (1,'event',$1,$1,$1,1,$2)")
        .bind(id.to_string()).bind(r#"{"version":999,"private":"never-print-envelope-canary"}"#).execute(&pool).await.unwrap();
    let archive = root.path().join("archive");
    let output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_memeloop-token-center"))
            .args(["request-history", "export-page", "--page-size", "1"])
            .env("MTC_KEY_PEPPER", "test-pepper-must-have-at-least-32-bytes")
            .env("MTC_SERVICE_TOKEN", "s".repeat(32))
            .env("MTC_DATABASE_URL", url)
            .env("MTC_ARCHIVE_BACKEND", "filesystem")
            .env("MTC_ARCHIVE_PATH", archive)
            .env("MTC_REQUEST_HISTORY_URL", "http://127.0.0.1:9")
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
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains("never-print-envelope-canary"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("never-print-envelope-canary"));
    let report: serde_json::Value = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value.get("status").is_some())
        .unwrap();
    assert_eq!(report["status"], "failed");
    assert_eq!(report["stage"], "capture_materialize");
    assert_eq!(report["diagnostic"]["category"], "invalid");
    assert_eq!(report["acked"], 0);
    let retained: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_history_immutable_outbox WHERE revision=1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(retained, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_history_cli_transport_probe_uses_bootstrap_admin_identity() {
    let root = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        root.path().join("probe.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    drop(database);
    let archive_path = root.path().join("must-not-initialize-archive");
    let archive_check = archive_path.clone();
    let requests = Arc::new(Mutex::new(0));
    let observed = requests.clone();
    let router = Router::new().route(
        "/",
        post(move |headers: axum::http::HeaderMap, body: Bytes| {
            let observed = observed.clone();
            async move {
                assert_eq!(
                    headers.get("authorization").unwrap(),
                    "Basic YWRtaW46YWRtaW4tcGFzc3dvcmQ="
                );
                assert_eq!(headers.get("x-clickhouse-database").unwrap(), "synthetic");
                assert_eq!(body.as_ref(), b"SELECT 1 AS ok FORMAT JSONEachRow");
                *observed.lock().unwrap() += 1;
                "{\"ok\":1}\n"
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
            .args(["request-history", "probe-transport"])
            .env("MTC_KEY_PEPPER", "test-pepper-must-have-at-least-32-bytes")
            .env("MTC_SERVICE_TOKEN", "s".repeat(32))
            .env("MTC_DATABASE_URL", database_url)
            .env("MTC_ARCHIVE_BACKEND", "filesystem")
            .env("MTC_ARCHIVE_PATH", archive_path)
            .env("MTC_REQUEST_HISTORY_URL", endpoint)
            .env("MTC_REQUEST_HISTORY_DATABASE", "synthetic")
            .env("MTC_REQUEST_HISTORY_ADMIN_USER", "admin")
            .env("MTC_REQUEST_HISTORY_ADMIN_PASSWORD", "admin-password")
            .env("MTC_REQUEST_HISTORY_WRITER_USER", "different-writer")
            .env(
                "MTC_REQUEST_HISTORY_WRITER_PASSWORD",
                "different-writer-password",
            )
            .env("MTC_REQUEST_HISTORY_USER", "different-fallback")
            .env(
                "MTC_REQUEST_HISTORY_PASSWORD",
                "different-fallback-password",
            )
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
    assert_eq!(*requests.lock().unwrap(), 1);
    assert!(!archive_check.exists());
    let result = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value.get("transport_ready").is_some())
        .unwrap();
    assert_eq!(
        result,
        serde_json::json!({"transport_ready":true,"read_only":true})
    );
    for bytes in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(bytes);
        assert!(!text.contains("admin-password"));
        assert!(!text.contains("different-writer-password"));
    }
    server.abort();
}
