use std::io::Write;

use memeloop_token_center::{
    archive::ArchiveStore,
    config::Config,
    db::{CreateKeyInput, Database, NewRequest},
    model::KeyPolicy,
    session_archive_import::{SessionArchiveImportOptions, import_session_archive},
};
use rust_decimal::Decimal;
use serde_json::json;
use tempfile::NamedTempFile;
use uuid::Uuid;

#[tokio::test]
async fn archive_import_is_fail_closed_gap_only_and_idempotent() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database_path = directory.path().join("target.sqlite");
    let database_url = format!("sqlite://{}?mode=rwc", database_path.display());
    let db = Database::connect(&database_url)
        .await
        .expect("connect SQLite");
    db.migrate().await.expect("migrate target");
    let config = Config::for_test(database_url.clone());
    let archive = ArchiveStore::from_config(&config)
        .await
        .expect("memory archive");
    let issued = db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "archive-fixture".into(),
                principal_external_id: "linux-codex".into(),
                alias: "Linux Codex".into(),
                currency: "USD".into(),
                policy: KeyPolicy {
                    allowed_models: vec!["gpt-fixture".into()],
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::new(10, 0),
                idempotency_key: Some("archive-fixture-key".into()),
            },
            config.key_pepper.as_bytes(),
        )
        .await
        .expect("create fixture key");
    let key = db
        .authenticate_key(&issued.key, config.key_pepper.as_bytes())
        .await
        .expect("authenticate fixture key");
    let request_id = Uuid::now_v7();
    let event_hash = "e".repeat(64);
    let source_key_hash = "a".repeat(64);
    let source_request_id = "cpa-source-request-1";
    let started_at = 1_786_492_800_000_i64;
    db.record_request_started(NewRequest {
        request_id,
        key_id: key.key_id,
        tenant_id: key.tenant_id,
        protocol: "openai-responses".into(),
        model: "gpt-fixture".into(),
        request_object: format!("gap://cpamp/{event_hash}/request"),
        reservation_id: Uuid::now_v7(),
        upstream_account_id: None,
        model_route_id: None,
    })
    .await
    .expect("insert target request");

    sqlx::any::install_default_drivers();
    let pool = sqlx::AnyPool::connect(&database_url)
        .await
        .expect("connect fixture pool");
    sqlx::query(
        "UPDATE request_records SET created_at = $1, completed_at = $2, status_code = 200, response_object = $3 WHERE id = $4",
    )
    .bind(started_at)
    .bind(started_at + 1234)
    .bind(format!("gap://cpamp/{event_hash}"))
    .bind(request_id.to_string())
    .execute(&pool)
    .await
    .expect("complete target request");
    sqlx::query("UPDATE request_record_locators SET created_at = $1 WHERE id = $2")
        .bind(started_at)
        .bind(request_id.to_string())
        .execute(&pool)
        .await
        .expect("route target request through locator");
    sqlx::query(
        "INSERT INTO import_request_links (tenant_id, source, external_event_hash, external_request_id, source_key_hash, target_request_id, source_created_at, source_model, created_at) VALUES ($1, 'cpamp-usage-events-v1', $2, $3, $4, $5, $6, 'gpt-fixture', $7)",
    )
    .bind(key.tenant_id.to_string())
    .bind(&event_hash)
    .bind(source_request_id)
    .bind(&source_key_hash)
    .bind(request_id.to_string())
    .bind(started_at)
    .bind(started_at)
    .execute(&pool)
    .await
    .expect("insert exact CPAMP link");

    let good = json!({
        "schema_version": 2,
        "session_id": "durable-thread-1",
        "request_id": source_request_id,
        "started_at": "2026-08-12T00:00:00Z",
        "completed_at": "2026-08-12T00:00:01.234Z",
        "key_id": source_key_hash,
        "credential_hash": source_key_hash,
        "requested_model": "gpt-fixture",
        "model": "gpt-fixture",
        "outcome": "succeeded",
        "status_code": 200,
        "facets": {"turn.id": ["turn-1"], "client": ["Codex"]},
        "request": {"model":"gpt-fixture","input":[{"role":"user","content":"fixture prompt"}]},
        "response": {"id":"response-1","output":[{"role":"assistant","content":"fixture answer"}]}
    });
    let bad = json!({
        "schema_version": 2,
        "session_id": "durable-thread-2",
        "request_id": "unmapped-request",
        "started_at": "2026-08-12T00:00:00Z",
        "completed_at": "2026-08-12T00:00:01Z",
        "credential_hash": "f".repeat(64),
        "requested_model": "gpt-fixture",
        "request": {"input":"must not be written"},
        "response": {"output":"must not be written"}
    });
    let mixed = jsonl(&[good.clone(), bad]);
    let mixed_options = options(&mixed, true);
    let error = import_session_archive(&db, &archive, &mixed_options)
        .await
        .expect_err("unmapped batch must fail before writes");
    assert!(error.to_string().contains("stopped before writes"));
    let refs = db
        .request_archive_refs_for_tenant("archive-fixture", request_id)
        .await
        .expect("target references");
    assert!(refs.request_object.starts_with("gap://"));
    assert!(refs.response_object.unwrap().starts_with("gap://"));

    let protected_request_id = Uuid::now_v7();
    let protected_event_hash = "d".repeat(64);
    db.record_request_started(NewRequest {
        request_id: protected_request_id,
        key_id: key.key_id,
        tenant_id: key.tenant_id,
        protocol: "openai-responses".into(),
        model: "gpt-fixture".into(),
        request_object: format!("gap://cpamp/{protected_event_hash}/request"),
        reservation_id: Uuid::now_v7(),
        upstream_account_id: None,
        model_route_id: None,
    })
    .await
    .expect("insert protected target");
    sqlx::query(
        "UPDATE request_records SET created_at = $1, completed_at = $2, status_code = 200, response_object = $3 WHERE id = $4",
    )
    .bind(started_at + 2000)
    .bind(started_at + 3000)
    .bind(r#"inline-json:{"id":"live-response"}"#)
    .bind(protected_request_id.to_string())
    .execute(&pool)
    .await
    .expect("complete protected target");
    sqlx::query("UPDATE request_record_locators SET created_at = $1 WHERE id = $2")
        .bind(started_at + 2000)
        .bind(protected_request_id.to_string())
        .execute(&pool)
        .await
        .expect("route protected target through locator");
    sqlx::query(
        "INSERT INTO import_request_links (tenant_id, source, external_event_hash, external_request_id, source_key_hash, target_request_id, source_created_at, source_model, created_at) VALUES ($1, 'cpamp-usage-events-v1', $2, 'cpa-source-request-2', $3, $4, $5, 'gpt-fixture', $6)",
    )
    .bind(key.tenant_id.to_string())
    .bind(&protected_event_hash)
    .bind(&source_key_hash)
    .bind(protected_request_id.to_string())
    .bind(started_at + 2000)
    .bind(started_at + 2000)
    .execute(&pool)
    .await
    .expect("insert protected CPAMP link");
    let protected = json!({
        "schema_version": 2,
        "session_id": "durable-thread-2",
        "request_id": "cpa-source-request-2",
        "started_at": "2026-08-12T00:00:02Z",
        "completed_at": "2026-08-12T00:00:03Z",
        "credential_hash": source_key_hash,
        "requested_model": "gpt-fixture",
        "model": "gpt-fixture",
        "outcome": "succeeded",
        "status_code": 200,
        "request": {"input":"request would otherwise be archived"},
        "response": {"output":"must not replace the live inline response"}
    });
    let prospective_objects = [
        content_location(&good["request"]),
        content_location(&good["response"]),
        content_location(&protected["request"]),
        content_location(&protected["response"]),
    ];
    let protected_mixed = jsonl(&[good.clone(), protected]);

    let error = import_session_archive(&db, &archive, &options(&protected_mixed, false))
        .await
        .expect_err("dry run must preflight protected locators");
    assert!(error.to_string().contains("refused to overwrite"));
    assert_failed_batch_unchanged(
        &db,
        &archive,
        &pool,
        request_id,
        protected_request_id,
        &prospective_objects,
    )
    .await;

    let error = import_session_archive(&db, &archive, &options(&protected_mixed, true))
        .await
        .expect_err("later protected locator must fail apply before writes");
    assert!(error.to_string().contains("refused to overwrite"));
    assert_failed_batch_unchanged(
        &db,
        &archive,
        &pool,
        request_id,
        protected_request_id,
        &prospective_objects,
    )
    .await;

    let input = jsonl(&[good]);
    let dry_run = import_session_archive(&db, &archive, &options(&input, false))
        .await
        .expect("dry run");
    assert_eq!(dry_run.mapped, 1);
    assert_eq!(dry_run.imported, 0);
    let applied = import_session_archive(&db, &archive, &options(&input, true))
        .await
        .expect("apply import");
    assert_eq!(applied.imported, 1);
    let refs = db
        .request_archive_refs_for_tenant("archive-fixture", request_id)
        .await
        .expect("imported references");
    assert!(refs.request_object.starts_with("objects/blake3/"));
    assert!(
        refs.response_object
            .as_deref()
            .unwrap()
            .starts_with("objects/blake3/")
    );
    assert_eq!(
        archive
            .get(&refs.request_object)
            .await
            .expect("request body"),
        serde_json::to_vec(
            &json!({"model":"gpt-fixture","input":[{"role":"user","content":"fixture prompt"}]})
        )
        .unwrap()
    );
    let observation_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM conversation_observations WHERE request_id = $1 AND explicit_session_id = 'durable-thread-1'",
    )
    .bind(request_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("conversation observation");
    assert_eq!(observation_count, 1);

    let replay = import_session_archive(&db, &archive, &options(&input, true))
        .await
        .expect("idempotent replay");
    assert_eq!(replay.replayed, 1);
    assert_eq!(replay.imported, 0);

    let old_unmapped = json!({
        "schema_version": 2,
        "session_id": "old-session",
        "request_id": "old-unmapped-request",
        "started_at": "2026-08-10T00:00:00Z",
        "completed_at": "2026-08-10T00:00:01Z",
        "credential_hash": "f".repeat(64),
        "requested_model": "gpt-fixture",
        "request": {"input":"outside overlap"},
        "response": {"output":"outside overlap"}
    });
    let old_input = jsonl(&[old_unmapped]);
    let skipped = import_session_archive(&db, &archive, &options(&old_input, true))
        .await
        .expect("checkpoint excludes records before the overlap window");
    assert_eq!(skipped.before_overlap, 1);
    assert_eq!(skipped.unmapped, 0);
}

async fn assert_failed_batch_unchanged(
    db: &Database,
    archive: &ArchiveStore,
    pool: &sqlx::AnyPool,
    gap_request_id: Uuid,
    protected_request_id: Uuid,
    prospective_objects: &[String],
) {
    let gap_refs = db
        .request_archive_refs_for_tenant("archive-fixture", gap_request_id)
        .await
        .expect("gap target references");
    assert!(gap_refs.request_object.starts_with("gap://"));
    assert!(gap_refs.response_object.unwrap().starts_with("gap://"));

    let protected_refs = db
        .request_archive_refs_for_tenant("archive-fixture", protected_request_id)
        .await
        .expect("protected target references");
    assert!(protected_refs.request_object.starts_with("gap://"));
    assert_eq!(
        protected_refs.response_object.as_deref(),
        Some(r#"inline-json:{"id":"live-response"}"#)
    );

    let imported: i64 = sqlx::query_scalar("SELECT count(*) FROM session_archive_import_records")
        .fetch_one(pool)
        .await
        .expect("count import records");
    let checkpoints: i64 =
        sqlx::query_scalar("SELECT count(*) FROM session_archive_import_checkpoints")
            .fetch_one(pool)
            .await
            .expect("count import checkpoints");
    let observations: i64 = sqlx::query_scalar("SELECT count(*) FROM conversation_observations")
        .fetch_one(pool)
        .await
        .expect("count conversation observations");
    assert_eq!(imported, 0);
    assert_eq!(checkpoints, 0);
    assert_eq!(observations, 0);
    for location in prospective_objects {
        assert!(
            archive.get(location).await.is_err(),
            "preflight failure must not create {location}"
        );
    }
}

fn content_location(value: &serde_json::Value) -> String {
    let body = match value {
        serde_json::Value::String(value) => value.as_bytes().to_vec(),
        value => serde_json::to_vec(value).expect("serialize fixture payload"),
    };
    let digest = blake3::hash(&body).to_hex().to_string();
    format!("objects/blake3/{}/{digest}", &digest[..2])
}

fn jsonl(values: &[serde_json::Value]) -> NamedTempFile {
    let mut file = NamedTempFile::new().expect("JSONL fixture");
    for value in values {
        serde_json::to_writer(&mut file, value).expect("write JSON fixture");
        writeln!(file).expect("terminate JSONL line");
    }
    file.flush().expect("flush JSONL fixture");
    file
}

fn options(input: &NamedTempFile, apply: bool) -> SessionArchiveImportOptions<'_> {
    SessionArchiveImportOptions {
        input: input.path(),
        tenant_external_id: "archive-fixture",
        cpamp_source: "cpamp-usage-events-v1",
        archive_source: "cpa-session-archive-v2",
        overlap_ms: 86_400_000,
        time_tolerance_ms: 5_000,
        max_line_bytes: 1024 * 1024,
        allow_unmapped: false,
        apply,
    }
}
