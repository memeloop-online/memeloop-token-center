use std::io::Write;

use memeloop_token_center::{
    archive::ArchiveStore,
    config::Config,
    conversation::ConversationHints,
    db::{
        CreateKeyInput, Database, NewRequest, SessionArchiveCommitInput, SessionArchiveMatchInput,
    },
    error::AppError,
    model::KeyPolicy,
    session_archive_import::{SessionArchiveImportOptions, import_session_archive},
};
use rust_decimal::Decimal;
use serde_json::json;
use tempfile::NamedTempFile;
use uuid::Uuid;

const TEST_IDENTITY_PROOF_KIND: &str = "test-exact-target-v1";
const TEST_IDENTITY_PROOF_DIGEST: &str =
    "4444444444444444444444444444444444444444444444444444444444444444";
const TEST_CORRELATION_PROOF_DIGEST: &str =
    "5555555555555555555555555555555555555555555555555555555555555555";
const TEST_RECORD_DIGEST: &str = "6666666666666666666666666666666666666666666666666666666666666666";
const TEST_OTHER_RECORD_DIGEST: &str =
    "7777777777777777777777777777777777777777777777777777777777777777";

#[tokio::test]
async fn session_archive_schema_precondition_is_read_only() {
    let directory = tempfile::tempdir().expect("schema precondition directory");
    let database_path = directory.path().join("unmigrated.sqlite");
    std::fs::write(&database_path, []).expect("create empty unmigrated database");
    let database_url = format!("sqlite://{}?mode=rwc", database_path.display());
    let db = Database::connect(&database_url)
        .await
        .expect("connect unmigrated SQLite");
    let before = std::fs::read(&database_path).expect("read database before precondition");
    let error = db
        .ensure_session_archive_import_schema()
        .await
        .expect_err("unmigrated database must be rejected");
    assert!(error.to_string().contains("must be migrated"));
    let after = std::fs::read(&database_path).expect("read database after precondition");
    assert_eq!(after, before, "schema precondition must never perform DDL");
}

#[tokio::test]
async fn archive_import_is_fail_closed_gap_only_and_idempotent() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database_path = directory.path().join("target.sqlite");
    let database_url = format!("sqlite://{}?mode=rwc", database_path.display());
    let db = Database::connect(&database_url)
        .await
        .expect("connect SQLite");
    db.migrate().await.expect("migrate target");
    db.ensure_session_archive_import_schema()
        .await
        .expect("migrated schema precondition");
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
    let missing_legacy_hash = json!({
        "schema_version": 1,
        "session_id": "durable-thread-2",
        "request_id": "missing-v1-key-hash",
        "started_at": "2026-08-12T00:00:00Z",
        "completed_at": "2026-08-12T00:00:01Z",
        "requested_model": "gpt-fixture",
        "request": {"input":"must not be written"},
        "response": {"output":"must not be written"}
    });
    let invalid_legacy_hash = json!({
        "schema_version": 1,
        "session_id": "durable-thread-3",
        "request_id": "invalid-v1-key-hash",
        "started_at": "2026-08-12T00:00:00Z",
        "completed_at": "2026-08-12T00:00:01Z",
        "key_id": "Linux Codex",
        "requested_model": "gpt-fixture",
        "request": {"input":"must not be written"},
        "response": {"output":"must not be written"}
    });
    let mixed = jsonl(&[good.clone(), missing_legacy_hash, invalid_legacy_hash]);
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

    let mut unsafe_override = options(&mixed, true);
    unsafe_override.allow_unmapped = true;
    let error = import_session_archive(&db, &archive, &unsafe_override)
        .await
        .expect_err("allow-unmapped must never be usable for apply");
    assert!(error.to_string().contains("diagnostic-only"));
    let refs = db
        .request_archive_refs_for_tenant("archive-fixture", request_id)
        .await
        .expect("target references after refused override");
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

    let input = jsonl(std::slice::from_ref(&good));
    let dry_run = import_session_archive(&db, &archive, &options(&input, false))
        .await
        .expect("dry run");
    assert_eq!(dry_run.mapped, 1);
    assert_eq!(dry_run.imported, 0);
    let sealed_bytes = std::fs::read(input.path()).expect("read sealed fixture");
    assert_eq!(dry_run.input_size_bytes, sealed_bytes.len() as u64);
    assert_eq!(
        dry_run.input_blake3,
        blake3::hash(&sealed_bytes).to_hex().to_string()
    );
    #[cfg(unix)]
    assert_ne!(dry_run.input_inode, 0);
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

    sqlx::query(
        "DELETE FROM session_archive_correlations WHERE source = 'cpa-session-archive-v2' AND external_request_id = $1",
    )
    .bind(source_request_id)
    .execute(&pool)
    .await
    .expect("simulate provenance imported before migration 0028");
    let replay = import_session_archive(&db, &archive, &options(&input, true))
        .await
        .expect("idempotent replay backfills exact correlation");
    assert_eq!(replay.replayed, 1);
    assert_eq!(replay.imported, 0);
    let exact_correlation: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_archive_correlations WHERE source = 'cpa-session-archive-v2' AND external_request_id = $1 AND disposition = 'exact'",
    )
    .bind(source_request_id)
    .fetch_one(&pool)
    .await
    .expect("backfilled exact correlation");
    assert_eq!(exact_correlation, 1);

    let archive_only = json!({
        "schema_version": 2,
        "session_id": "durable-thread-unlinked",
        "request_id": "archive-only-request-1",
        "started_at": "2026-08-12T00:00:06Z",
        "completed_at": "2026-08-12T00:00:07.500Z",
        "credential_hash": source_key_hash,
        "requested_model": "gpt-fixture",
        "model": "resolved-gpt-fixture",
        "request_path": "/v1/responses",
        "outcome": "rate limited",
        "status_code": 429,
        "facets": {"turn.id": ["turn-unlinked"], "client": ["Codex"]},
        "request": {"model":"gpt-fixture","input":"archive-only prompt"},
        "response": {"response":{"usage":{"input_tokens":11,"output_tokens":7}}}
    });
    let mixed_exact_unlinked = jsonl(&[good.clone(), archive_only]);
    let live_counts_before: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM request_records), (SELECT COUNT(*) FROM request_stats_facts), (SELECT COUNT(*) FROM usage_reservations), (SELECT COUNT(*) FROM ledger_entries)",
    )
    .fetch_one(&pool)
    .await
    .expect("live and billing counts before archive-only import");
    let mixed_result = import_session_archive(&db, &archive, &options(&mixed_exact_unlinked, true))
        .await
        .expect("mixed exact replay and archive-only apply");
    assert_eq!(mixed_result.mapped, 2);
    assert_eq!(mixed_result.replayed, 1);
    assert_eq!(mixed_result.imported, 1);
    let dispositions: (i64, i64) = sqlx::query_as(
        "SELECT SUM(CASE WHEN disposition = 'exact' THEN 1 ELSE 0 END), SUM(CASE WHEN disposition = 'unlinked' THEN 1 ELSE 0 END) FROM session_archive_correlations WHERE source = 'cpa-session-archive-v2'",
    )
    .fetch_one(&pool)
    .await
    .expect("mixed correlation dispositions");
    assert_eq!(dispositions, (1, 1));
    let archive_only_metadata: (String, String, Option<i64>, Option<i64>, i64, i64, Option<String>) =
        sqlx::query_as(
            "SELECT protocol, model, status_code, duration_ms, input_tokens, output_tokens, error_code FROM session_archive_unlinked_requests WHERE source = 'cpa-session-archive-v2' AND external_request_id = 'archive-only-request-1'",
        )
        .fetch_one(&pool)
        .await
        .expect("archive-only metadata");
    assert_eq!(
        archive_only_metadata,
        (
            "/v1/responses".into(),
            "gpt-fixture".into(),
            Some(429),
            Some(1_500),
            11,
            7,
            Some("rate_limited".into()),
        )
    );
    let live_counts_after: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM request_records), (SELECT COUNT(*) FROM request_stats_facts), (SELECT COUNT(*) FROM usage_reservations), (SELECT COUNT(*) FROM ledger_entries)",
    )
    .fetch_one(&pool)
    .await
    .expect("live and billing counts after archive-only import");
    assert_eq!(live_counts_after, live_counts_before);
    let mixed_checkpoint: i64 = sqlx::query_scalar(
        "SELECT imported_records FROM session_archive_import_checkpoints WHERE source = 'cpa-session-archive-v2'",
    )
    .fetch_one(&pool)
    .await
    .expect("mixed import checkpoint");
    assert_eq!(mixed_checkpoint, 2);

    let mixed_replay = import_session_archive(&db, &archive, &options(&mixed_exact_unlinked, true))
        .await
        .expect("mixed exact and archive-only replay");
    assert_eq!(mixed_replay.replayed, 2);
    assert_eq!(mixed_replay.imported, 0);
    let stable_counts: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM session_archive_correlations WHERE source = 'cpa-session-archive-v2'), (SELECT COUNT(*) FROM session_archive_unlinked_requests WHERE source = 'cpa-session-archive-v2'), (SELECT COUNT(*) FROM conversation_observations), (SELECT imported_records FROM session_archive_import_checkpoints WHERE source = 'cpa-session-archive-v2')",
    )
    .fetch_one(&pool)
    .await
    .expect("idempotent mixed counts");
    assert_eq!(stable_counts, (2, 1, 2, 2));

    let duplicate_one = Uuid::now_v7();
    let duplicate_one_hash = "b".repeat(64);
    insert_duplicate_archive_candidate(
        &db,
        &pool,
        key.key_id,
        key.tenant_id,
        duplicate_one,
        &duplicate_one_hash,
        source_request_id,
        &source_key_hash,
        started_at,
        7,
        3,
    )
    .await;
    let match_input = |input_tokens, output_tokens| SessionArchiveMatchInput {
        tenant_external_id: "archive-fixture",
        cpamp_source: "cpamp-usage-events-v1",
        archive_source: "token-disambiguation-test",
        external_request_id: source_request_id,
        started_at,
        requested_model: Some("gpt-fixture"),
        resolved_model: Some("gpt-fixture"),
        source_key_hash: &source_key_hash,
        input_tokens,
        output_tokens,
        record_digest: "token-disambiguation-record",
        time_tolerance_ms: 0,
    };
    db.match_session_archive_request(match_input(None, None))
        .await
        .expect_err("multiple exact coordinates without archive usage must fail closed");
    let usage_match = db
        .match_session_archive_request(match_input(Some(7), Some(3)))
        .await
        .expect("explicit complete token pair selects one candidate");
    assert_eq!(usage_match.target_request_id, duplicate_one);
    let no_hints = ConversationHints {
        session_id: None,
        turn_id: None,
        parent_turn_id: None,
        branch_id: None,
        compaction: false,
    };
    let commit_input = |record_digest| SessionArchiveCommitInput {
        tenant_external_id: "archive-fixture",
        archive_source: "stale-conflict-test",
        external_request_id: "stale-external-request",
        target: &usage_match,
        record_digest,
        request_digest: None,
        response_digest: None,
        request_object: None,
        response_object: None,
        request_json: None,
        conversation_hints: &no_hints,
        client_name: None,
        source_started_at: started_at,
        source_completed_at: None,
        identity_proof_kind: TEST_IDENTITY_PROOF_KIND,
        identity_proof_digest: TEST_IDENTITY_PROOF_DIGEST,
        correlation_proof_digest: TEST_CORRELATION_PROOF_DIGEST,
    };
    assert!(
        db.commit_session_archive_request(commit_input(TEST_RECORD_DIGEST))
            .await
            .expect("first stale-target commit")
    );
    let conflict = db
        .commit_session_archive_request(commit_input(TEST_OTHER_RECORD_DIGEST))
        .await
        .expect_err("a stale concurrent target with a different digest must fail");
    assert!(conflict.to_string().contains("changed while"));
    assert!(
        !db.commit_session_archive_request(commit_input(TEST_RECORD_DIGEST))
            .await
            .expect("an identical stale target is an idempotent replay")
    );

    let duplicate_two = Uuid::now_v7();
    let duplicate_two_hash = "9".repeat(64);
    insert_duplicate_archive_candidate(
        &db,
        &pool,
        key.key_id,
        key.tenant_id,
        duplicate_two,
        &duplicate_two_hash,
        source_request_id,
        &source_key_hash,
        started_at,
        7,
        3,
    )
    .await;
    db.match_session_archive_request(match_input(Some(7), Some(3)))
        .await
        .expect_err("equal token pairs must remain ambiguous");
    sqlx::query(
        "DELETE FROM import_request_links WHERE external_event_hash = $1 OR external_event_hash = $2",
    )
    .bind(&duplicate_one_hash)
    .bind(&duplicate_two_hash)
    .execute(&pool)
    .await
    .expect("remove ambiguity fixtures");

    let legacy_request_id = Uuid::now_v7();
    let legacy_event_hash = "c".repeat(64);
    db.record_request_started(NewRequest {
        request_id: legacy_request_id,
        key_id: key.key_id,
        tenant_id: key.tenant_id,
        protocol: "openai-responses".into(),
        model: "gpt-fixture".into(),
        request_object: format!("gap://cpamp/{legacy_event_hash}/request"),
        reservation_id: Uuid::now_v7(),
        upstream_account_id: None,
        model_route_id: None,
    })
    .await
    .expect("insert schema-v1 target");
    sqlx::query(
        "UPDATE request_records SET created_at = $1, completed_at = $2, status_code = 200, response_object = $3 WHERE id = $4",
    )
    .bind(started_at + 4000)
    .bind(started_at + 5000)
    .bind(format!("gap://cpamp/{legacy_event_hash}"))
    .bind(legacy_request_id.to_string())
    .execute(&pool)
    .await
    .expect("complete schema-v1 target");
    sqlx::query("UPDATE request_record_locators SET created_at = $1 WHERE id = $2")
        .bind(started_at + 4000)
        .bind(legacy_request_id.to_string())
        .execute(&pool)
        .await
        .expect("route schema-v1 target through locator");
    sqlx::query(
        "INSERT INTO import_request_links (tenant_id, source, external_event_hash, external_request_id, source_key_hash, target_request_id, source_created_at, source_model, created_at) VALUES ($1, 'cpamp-usage-events-v1', $2, 'cpa-source-request-v1', $3, $4, $5, 'gpt-fixture', $6)",
    )
    .bind(key.tenant_id.to_string())
    .bind(&legacy_event_hash)
    .bind(&source_key_hash)
    .bind(legacy_request_id.to_string())
    .bind(started_at + 4000)
    .bind(started_at + 4000)
    .execute(&pool)
    .await
    .expect("insert schema-v1 CPAMP link");
    // This is the exact field envelope emitted by cpa-session-archive v0.7.21:
    // schema 1 has key_id but no credential_hash or principal_id.
    let legacy_v1 = json!({
        "schema_version": 1,
        "session_id": "durable-thread-v1",
        "request_id": "cpa-source-request-v1",
        "started_at": "2026-08-12T00:00:04Z",
        "completed_at": "2026-08-12T00:00:05Z",
        "key_id": source_key_hash,
        "requested_model": "gpt-fixture",
        "model": "gpt-fixture",
        "outcome": "succeeded",
        "status_code": 200,
        "metadata": {"client": "Codex"},
        "facets": {"turn.id": ["turn-v1"]},
        "request": {"model":"gpt-fixture","input":"legacy schema prompt"},
        "response": null
    });
    let mixed_versions = jsonl(&[good, legacy_v1]);
    let mixed_apply = import_session_archive(&db, &archive, &options(&mixed_versions, true))
        .await
        .expect("mixed schema-v1/schema-v2 apply");
    assert_eq!(mixed_apply.replayed, 1);
    assert_eq!(mixed_apply.imported, 1);
    let legacy_refs = db
        .request_archive_refs_for_tenant("archive-fixture", legacy_request_id)
        .await
        .expect("schema-v1 imported references");
    assert!(legacy_refs.request_object.starts_with("objects/blake3/"));
    assert_eq!(
        legacy_refs.response_object.as_deref(),
        Some(format!("gap://cpamp/{legacy_event_hash}").as_str()),
        "a null archive response must preserve the existing locator"
    );

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

    let mutation_request_id = Uuid::now_v7();
    let mutation_event_hash = "8".repeat(64);
    let mutation_source_request_id = "source-mutates-during-plan";
    let mutation_started_at = started_at + 8_000;
    insert_duplicate_archive_candidate(
        &db,
        &pool,
        key.key_id,
        key.tenant_id,
        mutation_request_id,
        &mutation_event_hash,
        mutation_source_request_id,
        &source_key_hash,
        mutation_started_at,
        0,
        0,
    )
    .await;
    let mutation_record = json!({
        "schema_version": 2,
        "session_id": "source-mutation-session",
        "request_id": mutation_source_request_id,
        "started_at": "2026-08-12T00:00:08Z",
        "completed_at": "2026-08-12T00:00:09Z",
        "credential_hash": source_key_hash,
        "requested_model": "gpt-fixture",
        "model": "gpt-fixture",
        "outcome": "succeeded",
        "status_code": 200,
        "facets": {"client": ["Codex"]},
        "request": {"model":"gpt-fixture","input":"must remain uncommitted"},
        "response": {"id":"mutation-response","output":"orphan CAS is acceptable"}
    });
    let mut mutation_input = NamedTempFile::new().expect("source mutation input");
    serde_json::to_writer(&mut mutation_input, &mutation_record).expect("write mutation record");
    writeln!(mutation_input).expect("terminate mutation record");
    mutation_input
        .write_all(&vec![b' '; 8 * 1024 * 1024])
        .expect("write bounded planning tail");
    writeln!(mutation_input).expect("terminate planning tail");
    mutation_input.flush().expect("flush mutation input");
    let plan_directory = tempfile::tempdir().expect("isolated plan directory");
    let watched_directory = plan_directory.path().to_owned();
    let mutated_path = mutation_input.path().to_owned();
    let mutator = tokio::spawn(async move {
        for _ in 0..10_000 {
            let plan_exists = std::fs::read_dir(&watched_directory)
                .expect("read plan directory")
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().contains("archive-plan"));
            if plan_exists {
                let mut source = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&mutated_path)
                    .expect("open source for mutation");
                source
                    .write_all(b" ")
                    .expect("mutate source after pass one");
                source.flush().expect("flush source mutation");
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("sealed-plan file was never created");
    });
    let mut mutation_options = options(&mutation_input, true);
    mutation_options.archive_source = "source-mutation-test";
    mutation_options.plan_directory = plan_directory.path();
    mutation_options.max_line_bytes = 16 * 1024 * 1024;
    let mutation_error = import_session_archive(&db, &archive, &mutation_options)
        .await
        .expect_err("source mutation must abort before database apply");
    mutator.await.expect("join source mutator");
    assert!(
        mutation_error
            .to_string()
            .contains("changed after preflight")
    );
    let imported: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM session_archive_import_records WHERE source = 'source-mutation-test'",
    )
    .fetch_one(&pool)
    .await
    .expect("count mutation import records");
    let observations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM conversation_observations WHERE request_id = $1")
            .bind(mutation_request_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("count mutation observations");
    let checkpoints: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM session_archive_import_checkpoints WHERE source = 'source-mutation-test'",
    )
    .fetch_one(&pool)
    .await
    .expect("count mutation checkpoints");
    let correlations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM session_archive_correlations WHERE source = 'source-mutation-test'",
    )
    .fetch_one(&pool)
    .await
    .expect("count mutation correlations");
    let unlinked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM session_archive_unlinked_requests WHERE source = 'source-mutation-test'",
    )
    .fetch_one(&pool)
    .await
    .expect("count mutation archive-only rows");
    assert_eq!(
        (imported, observations, checkpoints, correlations, unlinked),
        (0, 0, 0, 0, 0)
    );
    let mutation_refs = db
        .request_archive_refs_for_tenant("archive-fixture", mutation_request_id)
        .await
        .expect("read unchanged mutation locators");
    assert_eq!(
        mutation_refs.request_object,
        format!("gap://cpamp/{mutation_event_hash}/request")
    );
    assert_eq!(
        mutation_refs.response_object.as_deref(),
        Some(format!("gap://cpamp/{mutation_event_hash}").as_str())
    );
    let orphan_request = content_location(&mutation_record["request"]);
    assert!(
        archive.get(&orphan_request).await.is_ok(),
        "planning may leave only a content-addressed orphan"
    );

    let recovery_started_at = started_at + 20_000;
    let recovery_fixtures = [
        (
            "a-long-request-fails-last",
            recovery_started_at,
            "1".repeat(64),
            Uuid::now_v7(),
        ),
        (
            "b-short-request",
            recovery_started_at + 1_000,
            "2".repeat(64),
            Uuid::now_v7(),
        ),
        (
            "c-short-request",
            recovery_started_at + 3_000,
            "3".repeat(64),
            Uuid::now_v7(),
        ),
    ];
    for (external_request_id, source_started_at, event_hash, target_request_id) in
        &recovery_fixtures
    {
        insert_duplicate_archive_candidate(
            &db,
            &pool,
            key.key_id,
            key.tenant_id,
            *target_request_id,
            event_hash,
            external_request_id,
            &source_key_hash,
            *source_started_at,
            0,
            0,
        )
        .await;
    }
    let recovery_record = |request_id: &str, started: &str, completed: &str| {
        json!({
            "schema_version": 2,
            "session_id": format!("recovery-{request_id}"),
            "request_id": request_id,
            "started_at": started,
            "completed_at": completed,
            "credential_hash": source_key_hash,
            "requested_model": "gpt-fixture",
            "model": "gpt-fixture",
            "outcome": "succeeded",
            "status_code": 200,
            "request": {"model":"gpt-fixture","input":request_id},
            "response": {"id":format!("response-{request_id}")}
        })
    };
    // The long request starts first but completes last. The sealed plan must use
    // completed-at checkpoint order, independent of JSONL or started-at order.
    let recovery_input = jsonl(&[
        recovery_record(
            "a-long-request-fails-last",
            "2026-08-12T00:00:20Z",
            "2026-08-12T00:02:00Z",
        ),
        recovery_record(
            "c-short-request",
            "2026-08-12T00:00:23Z",
            "2026-08-12T00:00:24Z",
        ),
        recovery_record(
            "b-short-request",
            "2026-08-12T00:00:21Z",
            "2026-08-12T00:00:22Z",
        ),
    ]);
    sqlx::query(
        "CREATE TRIGGER inject_archive_commit_failure BEFORE INSERT ON session_archive_import_records WHEN NEW.source = 'partial-recovery-test' AND NEW.external_request_id = 'a-long-request-fails-last' BEGIN SELECT RAISE(ABORT, 'injected archive commit failure'); END",
    )
    .execute(&pool)
    .await
    .expect("install deterministic commit failure");
    let mut recovery_options = options(&recovery_input, true);
    recovery_options.archive_source = "partial-recovery-test";
    recovery_options.overlap_ms = 0;
    import_session_archive(&db, &archive, &recovery_options)
        .await
        .expect_err("middle commit failure must stop this attempt");
    let first_attempt_imports: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM session_archive_import_records WHERE source = 'partial-recovery-test'",
    )
    .fetch_one(&pool)
    .await
    .expect("count first recovery attempt");
    assert_eq!(first_attempt_imports, 2);
    let recovery_checkpoint: (i64, i64) = sqlx::query_as(
        "SELECT watermark_ms, imported_records FROM session_archive_import_checkpoints WHERE source = 'partial-recovery-test'",
    )
    .fetch_one(&pool)
    .await
    .expect("read first recovery checkpoint");
    assert_eq!(recovery_checkpoint, (recovery_started_at + 4_000, 2));
    let long_refs = db
        .request_archive_refs_for_tenant("archive-fixture", recovery_fixtures[0].3)
        .await
        .expect("read failed long-request locator");
    assert!(long_refs.request_object.starts_with("gap://"));
    for fixture in &recovery_fixtures[1..] {
        let refs = db
            .request_archive_refs_for_tenant("archive-fixture", fixture.3)
            .await
            .expect("read committed short-request locator");
        assert!(refs.request_object.starts_with("objects/blake3/"));
    }

    sqlx::query("DROP TRIGGER inject_archive_commit_failure")
        .execute(&pool)
        .await
        .expect("remove deterministic commit failure");
    let recovered = import_session_archive(&db, &archive, &recovery_options)
        .await
        .expect("same sealed source completes remaining records");
    assert_eq!(recovered.before_overlap, 1);
    assert_eq!(recovered.replayed, 1);
    assert_eq!(recovered.imported, 1);
    let final_imports: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM session_archive_import_records WHERE source = 'partial-recovery-test'",
    )
    .fetch_one(&pool)
    .await
    .expect("count recovered imports");
    let final_observations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM conversation_observations WHERE request_id = $1 OR request_id = $2 OR request_id = $3",
    )
    .bind(recovery_fixtures[0].3.to_string())
    .bind(recovery_fixtures[1].3.to_string())
    .bind(recovery_fixtures[2].3.to_string())
    .fetch_one(&pool)
    .await
    .expect("count recovered observations");
    let final_checkpoint: (i64, i64) = sqlx::query_as(
        "SELECT watermark_ms, imported_records FROM session_archive_import_checkpoints WHERE source = 'partial-recovery-test'",
    )
    .fetch_one(&pool)
    .await
    .expect("read recovered checkpoint");
    assert_eq!(final_imports, 3);
    assert_eq!(final_observations, 3);
    assert_eq!(final_checkpoint, (recovery_started_at + 100_000, 3));
    let exact_replay = import_session_archive(&db, &archive, &recovery_options)
        .await
        .expect("exact replay remains idempotent");
    assert_eq!(exact_replay.before_overlap, 2);
    assert_eq!(exact_replay.replayed, 1);
    assert_eq!(exact_replay.imported, 0);
}

#[tokio::test]
async fn postgres_archive_import_lock_and_locator_cas_are_fail_closed() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let db = Database::connect_with_max(&database_url, 4)
        .await
        .expect("connect PostgreSQL");
    db.migrate().await.expect("migrate PostgreSQL");

    let lock_tenant = format!("archive-lock-{}", Uuid::now_v7());
    let first_lock = db
        .acquire_session_archive_import_lock(&lock_tenant, "archive-lock-test")
        .await
        .expect("acquire first archive lock");
    let conflict = match db
        .acquire_session_archive_import_lock(&lock_tenant, "archive-lock-test")
        .await
    {
        Err(error) => error,
        Ok(unexpected) => {
            unexpected.release().await.expect("release unexpected lock");
            panic!("a second importer must not overlap the first");
        }
    };
    assert!(conflict.to_string().contains("already running"));
    first_lock.release().await.expect("release first lock");
    db.acquire_session_archive_import_lock(&lock_tenant, "archive-lock-test")
        .await
        .expect("lock is reusable after release")
        .release()
        .await
        .expect("release reused lock");

    let unique = Uuid::now_v7();
    let tenant_external_id = format!("archive-cas-{unique}");
    let config = Config::for_test(database_url.clone());
    let archive = ArchiveStore::from_config(&config)
        .await
        .expect("create PostgreSQL archive store");
    let issued = db
        .create_key(
            CreateKeyInput {
                tenant_external_id: tenant_external_id.clone(),
                principal_external_id: "postgres-cas".into(),
                alias: "PostgreSQL CAS".into(),
                currency: "USD".into(),
                policy: KeyPolicy {
                    allowed_models: vec!["gpt-cas".into()],
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::new(10, 0),
                idempotency_key: Some(format!("archive-cas-key-{unique}")),
            },
            config.key_pepper.as_bytes(),
        )
        .await
        .expect("create PostgreSQL CAS key");
    let key = db
        .authenticate_key(&issued.key, config.key_pepper.as_bytes())
        .await
        .expect("authenticate PostgreSQL CAS key");
    let request_id = Uuid::now_v7();
    let event_hash = "7".repeat(64);
    let source_key_hash = "6".repeat(64);
    let started_at = 1_786_492_900_000_i64;
    db.record_request_started(NewRequest {
        request_id,
        key_id: key.key_id,
        tenant_id: key.tenant_id,
        protocol: "openai-responses".into(),
        model: "gpt-cas".into(),
        request_object: format!("gap://cpamp/{event_hash}/request"),
        reservation_id: Uuid::now_v7(),
        upstream_account_id: None,
        model_route_id: None,
    })
    .await
    .expect("insert PostgreSQL CAS target");
    let pool = sqlx::AnyPool::connect(&database_url)
        .await
        .expect("connect PostgreSQL CAS fixture pool");
    sqlx::query(
        "UPDATE request_records SET created_at = $1, completed_at = $1, status_code = 200, response_object = $2 WHERE id = $3",
    )
    .bind(started_at)
    .bind(format!("gap://cpamp/{event_hash}"))
    .bind(request_id.to_string())
    .execute(&pool)
    .await
    .expect("complete PostgreSQL CAS target");
    sqlx::query("UPDATE request_record_locators SET created_at = $1 WHERE id = $2")
        .bind(started_at)
        .bind(request_id.to_string())
        .execute(&pool)
        .await
        .expect("route PostgreSQL CAS target");
    sqlx::query(
        "INSERT INTO import_request_links (tenant_id, source, external_event_hash, external_request_id, source_key_hash, target_request_id, source_created_at, source_model, created_at) VALUES ($1, 'cpamp-usage-events-v1', $2, 'postgres-cas-external', $3, $4, $5, 'gpt-cas', $5)",
    )
    .bind(key.tenant_id.to_string())
    .bind(&event_hash)
    .bind(&source_key_hash)
    .bind(request_id.to_string())
    .bind(started_at)
    .execute(&pool)
    .await
    .expect("link PostgreSQL CAS target");
    let target = db
        .match_session_archive_request(SessionArchiveMatchInput {
            tenant_external_id: &tenant_external_id,
            cpamp_source: "cpamp-usage-events-v1",
            archive_source: "postgres-cas-archive",
            external_request_id: "postgres-cas-external",
            started_at,
            requested_model: Some("gpt-cas"),
            resolved_model: Some("gpt-cas"),
            source_key_hash: &source_key_hash,
            input_tokens: None,
            output_tokens: None,
            record_digest: "postgres-cas-digest",
            time_tolerance_ms: 0,
        })
        .await
        .expect("match PostgreSQL CAS target");

    let mut blocker = pool.begin().await.expect("begin locator blocker");
    sqlx::query(
        "SELECT id FROM request_records WHERE id = $1 AND created_at = $2 AND tenant_id = $3 FOR UPDATE",
    )
    .bind(request_id.to_string())
    .bind(started_at)
    .bind(key.tenant_id.to_string())
    .fetch_one(&mut *blocker)
    .await
    .expect("lock locator row");
    let replay_target = target.clone();
    let replay_tenant_external_id = tenant_external_id.clone();
    let commit_tenant_external_id = tenant_external_id.clone();
    let commit_db = db.clone();
    let commit_task = tokio::spawn(async move {
        let hints = ConversationHints {
            session_id: None,
            turn_id: None,
            parent_turn_id: None,
            branch_id: None,
            compaction: false,
        };
        commit_db
            .commit_session_archive_request(SessionArchiveCommitInput {
                tenant_external_id: &commit_tenant_external_id,
                archive_source: "postgres-cas-archive",
                external_request_id: "postgres-cas-external",
                target: &target,
                record_digest: TEST_RECORD_DIGEST,
                request_digest: Some(TEST_OTHER_RECORD_DIGEST),
                response_digest: None,
                request_object: Some("objects/blake3/aa/archive-request"),
                response_object: None,
                request_json: None,
                conversation_hints: &hints,
                client_name: None,
                source_started_at: started_at,
                source_completed_at: Some(started_at + 5_000),
                identity_proof_kind: TEST_IDENTITY_PROOF_KIND,
                identity_proof_digest: TEST_IDENTITY_PROOF_DIGEST,
                correlation_proof_digest: TEST_CORRELATION_PROOF_DIGEST,
            })
            .await
    });
    let mut update_is_blocked = false;
    for _ in 0..100 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() AND state = 'active' AND wait_event_type = 'Lock' AND query LIKE 'UPDATE request_records SET request_object = $1, response_object = $2 WHERE id = $3%'",
        )
        .fetch_one(&pool)
        .await
        .expect("inspect PostgreSQL CAS waiter");
        if waiting > 0 {
            update_is_blocked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        update_is_blocked,
        "commit must reach the blocked CAS update"
    );
    let protected_locator = r#"inline-json:{"live":true}"#;
    sqlx::query(
        "UPDATE request_records SET request_object = $1 WHERE id = $2 AND created_at = $3 AND tenant_id = $4",
    )
    .bind(protected_locator)
    .bind(request_id.to_string())
    .bind(started_at)
    .bind(key.tenant_id.to_string())
    .execute(&mut *blocker)
    .await
    .expect("concurrently protect locator");
    blocker.commit().await.expect("commit protected locator");
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), commit_task)
        .await
        .expect("CAS commit completes after blocker")
        .expect("join CAS commit")
        .expect_err("CAS must reject a concurrently protected locator");
    assert!(
        matches!(error, AppError::BadRequest(ref message) if message == "archive target changed after preflight"),
        "unexpected CAS error: {error:?}"
    );
    let stored_locator: String =
        sqlx::query_scalar("SELECT request_object FROM request_records WHERE id = $1")
            .bind(request_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("read protected locator");
    assert_eq!(stored_locator, protected_locator);
    let import_records: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM session_archive_import_records WHERE tenant_id = $1 AND source = 'postgres-cas-archive' AND external_request_id = 'postgres-cas-external'",
    )
    .bind(key.tenant_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("count rolled-back import records");
    assert_eq!(import_records, 0);

    // Once the concurrent owner restores the expected importable gap locator, the
    // exact same sealed-plan row must converge. A further overlap replay is a no-op.
    let original_gap_locator = format!("gap://cpamp/{event_hash}/request");
    sqlx::query(
        "UPDATE request_records SET request_object = $1 WHERE id = $2 AND created_at = $3 AND tenant_id = $4",
    )
    .bind(&original_gap_locator)
    .bind(request_id.to_string())
    .bind(started_at)
    .bind(key.tenant_id.to_string())
    .execute(&pool)
    .await
    .expect("restore importable locator after resolved concurrent change");
    let replay_hints = ConversationHints::default();
    let replay_input = || SessionArchiveCommitInput {
        tenant_external_id: &replay_tenant_external_id,
        archive_source: "postgres-cas-archive",
        external_request_id: "postgres-cas-external",
        target: &replay_target,
        record_digest: TEST_RECORD_DIGEST,
        request_digest: Some(TEST_OTHER_RECORD_DIGEST),
        response_digest: None,
        request_object: Some("objects/blake3/aa/archive-request"),
        response_object: None,
        request_json: None,
        conversation_hints: &replay_hints,
        client_name: None,
        source_started_at: started_at,
        source_completed_at: Some(started_at + 5_000),
        identity_proof_kind: TEST_IDENTITY_PROOF_KIND,
        identity_proof_digest: TEST_IDENTITY_PROOF_DIGEST,
        correlation_proof_digest: TEST_CORRELATION_PROOF_DIGEST,
    };
    assert!(
        db.commit_session_archive_request(replay_input())
            .await
            .expect("replay failed PostgreSQL row")
    );
    assert!(
        !db.commit_session_archive_request(replay_input())
            .await
            .expect("idempotently replay imported PostgreSQL row")
    );
    let recovered: (String, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT r.request_object, (SELECT COUNT(*) FROM session_archive_import_records i WHERE i.tenant_id = r.tenant_id AND i.source = 'postgres-cas-archive' AND i.external_request_id = 'postgres-cas-external'), (SELECT COUNT(*) FROM session_archive_correlations c WHERE c.tenant_id = r.tenant_id AND c.source = 'postgres-cas-archive' AND c.external_request_id = 'postgres-cas-external'), (SELECT imported_records FROM session_archive_import_checkpoints p WHERE p.tenant_id = r.tenant_id AND p.source = 'postgres-cas-archive'), (SELECT watermark_ms FROM session_archive_import_checkpoints p WHERE p.tenant_id = r.tenant_id AND p.source = 'postgres-cas-archive') FROM request_records r WHERE r.id = $1 AND r.created_at = $2",
    )
    .bind(request_id.to_string())
    .bind(started_at)
    .fetch_one(&pool)
    .await
    .expect("read recovered PostgreSQL import state");
    assert_eq!(
        recovered,
        (
            "objects/blake3/aa/archive-request".to_owned(),
            1,
            1,
            1,
            started_at + 5_000,
        )
    );

    // The target cursor is completed-at based. A late record that started before
    // that cursor must still import, then become an idempotent overlap replay.
    let archive_record = |request_id: &str, started: i64, completed: i64| {
        json!({
            "schema_version": 2,
            "session_id": format!("postgres-late-{request_id}"),
            "request_id": request_id,
            "started_at": rfc3339_millis(started),
            "completed_at": rfc3339_millis(completed),
            "credential_hash": source_key_hash,
            "requested_model": "gpt-cas",
            "model": "gpt-cas",
            "outcome": "succeeded",
            "status_code": 200,
            "request": {"model":"gpt-cas","input":request_id},
            "response": {"id":format!("response-{request_id}")}
        })
    };
    let late = archive_record(
        "postgres-late-completion",
        started_at - 1_000,
        started_at + 6_000,
    );
    let late_input = jsonl(std::slice::from_ref(&late));
    let late_options = SessionArchiveImportOptions {
        input: late_input.path(),
        plan_directory: late_input
            .path()
            .parent()
            .expect("PostgreSQL plan directory"),
        tenant_external_id: &tenant_external_id,
        cpamp_source: "cpamp-usage-events-v1",
        archive_source: "postgres-cas-archive",
        overlap_ms: 0,
        time_tolerance_ms: 5_000,
        max_line_bytes: 1024 * 1024,
        max_plan_bytes: 16 * 1024 * 1024,
        allow_unmapped: false,
        apply: true,
    };
    let late_applied = import_session_archive(&db, &archive, &late_options)
        .await
        .expect("import late-completed PostgreSQL record");
    assert_eq!(late_applied.before_overlap, 0);
    assert_eq!(late_applied.imported, 1);

    let short = archive_record(
        "postgres-short-completion",
        started_at + 7_000,
        started_at + 8_000,
    );
    let long = archive_record(
        "postgres-long-completion",
        started_at + 6_500,
        started_at + 10_000,
    );
    // PostgreSQL verifies the same crash boundary as SQLite: source/start order
    // puts the long record first, while checkpoint order must commit short first.
    let overlap_input = jsonl(&[long, late, short]);
    let overlap_options = SessionArchiveImportOptions {
        input: overlap_input.path(),
        plan_directory: overlap_input
            .path()
            .parent()
            .expect("PostgreSQL overlap plan directory"),
        tenant_external_id: &tenant_external_id,
        cpamp_source: "cpamp-usage-events-v1",
        archive_source: "postgres-cas-archive",
        overlap_ms: 0,
        time_tolerance_ms: 5_000,
        max_line_bytes: 1024 * 1024,
        max_plan_bytes: 16 * 1024 * 1024,
        allow_unmapped: false,
        apply: true,
    };
    sqlx::query(
        "CREATE FUNCTION inject_postgres_archive_failure() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.source = 'postgres-cas-archive' AND NEW.external_request_id = 'postgres-long-completion' THEN RAISE EXCEPTION 'injected archive commit failure'; END IF; RETURN NEW; END $$",
    )
    .execute(&pool)
    .await
    .expect("create PostgreSQL archive failure function");
    sqlx::query(
        "CREATE TRIGGER inject_postgres_archive_failure BEFORE INSERT ON session_archive_correlations FOR EACH ROW EXECUTE FUNCTION inject_postgres_archive_failure()",
    )
    .execute(&pool)
    .await
    .expect("create PostgreSQL archive failure trigger");
    import_session_archive(&db, &archive, &overlap_options)
        .await
        .expect_err("long completion failure stops after smaller checkpoint records");
    let failed_cursor: (i64, i64, i64) = sqlx::query_as(
        "SELECT watermark_ms, imported_records, (SELECT COUNT(*) FROM session_archive_unlinked_requests WHERE tenant_id = $1 AND source = 'postgres-cas-archive') FROM session_archive_import_checkpoints WHERE tenant_id = $1 AND source = 'postgres-cas-archive'",
    )
    .bind(key.tenant_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("read PostgreSQL cursor after injected failure");
    assert_eq!(failed_cursor, (started_at + 8_000, 3, 2));
    sqlx::query("DROP TRIGGER inject_postgres_archive_failure ON session_archive_correlations")
        .execute(&pool)
        .await
        .expect("drop PostgreSQL archive failure trigger");
    sqlx::query("DROP FUNCTION inject_postgres_archive_failure()")
        .execute(&pool)
        .await
        .expect("drop PostgreSQL archive failure function");

    let overlap_applied = import_session_archive(&db, &archive, &overlap_options)
        .await
        .expect("recover long-completed PostgreSQL record");
    assert_eq!(overlap_applied.before_overlap, 1);
    assert_eq!(overlap_applied.replayed, 1);
    assert_eq!(overlap_applied.imported, 1);

    let final_replay = import_session_archive(&db, &archive, &overlap_options)
        .await
        .expect("skip old completion and replay boundary record");
    assert_eq!(final_replay.before_overlap, 2);
    assert_eq!(final_replay.replayed, 1);
    assert_eq!(final_replay.imported, 0);
    let final_cursor: (i64, i64, i64) = sqlx::query_as(
        "SELECT watermark_ms, imported_records, (SELECT COUNT(*) FROM session_archive_unlinked_requests WHERE tenant_id = $1 AND source = 'postgres-cas-archive') FROM session_archive_import_checkpoints WHERE tenant_id = $1 AND source = 'postgres-cas-archive'",
    )
    .bind(key.tenant_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("read final PostgreSQL completed-at cursor");
    assert_eq!(final_cursor, (started_at + 10_000, 4, 3));
}

#[allow(clippy::too_many_arguments)]
async fn insert_duplicate_archive_candidate(
    db: &Database,
    pool: &sqlx::AnyPool,
    key_id: Uuid,
    tenant_id: Uuid,
    request_id: Uuid,
    event_hash: &str,
    external_request_id: &str,
    source_key_hash: &str,
    started_at: i64,
    input_tokens: i64,
    output_tokens: i64,
) {
    db.record_request_started(NewRequest {
        request_id,
        key_id,
        tenant_id,
        protocol: "openai-responses".into(),
        model: "gpt-fixture".into(),
        request_object: format!("gap://cpamp/{event_hash}/request"),
        reservation_id: Uuid::now_v7(),
        upstream_account_id: None,
        model_route_id: None,
    })
    .await
    .expect("insert duplicate candidate");
    sqlx::query(
        "UPDATE request_records SET created_at = $1, completed_at = $1, status_code = 200, input_tokens = $2, output_tokens = $3, response_object = $4 WHERE id = $5",
    )
    .bind(started_at)
    .bind(input_tokens)
    .bind(output_tokens)
    .bind(format!("gap://cpamp/{event_hash}"))
    .bind(request_id.to_string())
    .execute(pool)
    .await
    .expect("complete duplicate candidate");
    sqlx::query("UPDATE request_record_locators SET created_at = $1 WHERE id = $2")
        .bind(started_at)
        .bind(request_id.to_string())
        .execute(pool)
        .await
        .expect("route duplicate candidate through locator");
    sqlx::query(
        "INSERT INTO import_request_links (tenant_id, source, external_event_hash, external_request_id, source_key_hash, target_request_id, source_created_at, source_model, created_at) VALUES ($1, 'cpamp-usage-events-v1', $2, $3, $4, $5, $6, 'gpt-fixture', $6)",
    )
    .bind(tenant_id.to_string())
    .bind(event_hash)
    .bind(external_request_id)
    .bind(source_key_hash)
    .bind(request_id.to_string())
    .bind(started_at)
    .execute(pool)
    .await
    .expect("link duplicate candidate");
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
    let correlations: i64 = sqlx::query_scalar("SELECT count(*) FROM session_archive_correlations")
        .fetch_one(pool)
        .await
        .expect("count archive correlations");
    let unlinked: i64 =
        sqlx::query_scalar("SELECT count(*) FROM session_archive_unlinked_requests")
            .fetch_one(pool)
            .await
            .expect("count archive-only requests");
    assert_eq!(imported, 0);
    assert_eq!(checkpoints, 0);
    assert_eq!(observations, 0);
    assert_eq!(correlations, 0);
    assert_eq!(unlinked, 0);
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

fn rfc3339_millis(value: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(value)
        .expect("fixture timestamp")
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn options(input: &NamedTempFile, apply: bool) -> SessionArchiveImportOptions<'_> {
    SessionArchiveImportOptions {
        input: input.path(),
        plan_directory: input.path().parent().expect("fixture plan directory"),
        tenant_external_id: "archive-fixture",
        cpamp_source: "cpamp-usage-events-v1",
        archive_source: "cpa-session-archive-v2",
        overlap_ms: 86_400_000,
        time_tolerance_ms: 5_000,
        max_line_bytes: 1024 * 1024,
        max_plan_bytes: 16 * 1024 * 1024,
        allow_unmapped: false,
        apply,
    }
}
