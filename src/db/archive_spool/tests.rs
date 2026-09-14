use super::*;

#[tokio::test]
async fn compressed_buffered_capture_refunds_to_exact_ciphertext_in_the_same_transaction() {
    let (_dir, db, id) = fixture().await;
    let pepper = b"compressed-budget-test-pepper-over-32-bytes";
    let body = bytes::Bytes::from(
        serde_json::to_vec(&vec![
            serde_json::json!({
                "role": "assistant",
                "content": "synthetic repeated JSON content for deterministic accounting",
            });
            2_000
        ])
        .unwrap(),
    );
    let archive = crate::response_archive_spool::BufferedArchive::new(
        id,
        BufferedArchivePurpose::Request,
        &body,
        pepper,
        true,
    )
    .unwrap();
    let legacy_upper = body
        .chunks(crate::response_archive_spool::CHUNK_BYTES)
        .try_fold(SPOOL_OVERHEAD, |total, bytes| {
            total.checked_add(archive.sealed_len(bytes.len())? as i64 + CHUNK_OVERHEAD)
        })
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_compressed_refund BEFORE UPDATE OF cipher_bytes ON request_archive_spools WHEN NEW.cipher_bytes < OLD.cipher_bytes BEGIN SELECT RAISE(ABORT, 'refund failure'); END",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    let prepared = archive.prepare_first_batch().await.unwrap();
    let (mut tx, now) = db.spool_transaction().await.unwrap();
    assert!(
        db.capture_buffered_archive_body_in_transaction(&mut tx, now, &archive, Some(prepared))
            .await
            .is_err()
    );
    drop(tx);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM request_archive_spools WHERE request_id = $1",
        )
        .bind(id.request_id.to_string())
        .fetch_one(&db.pool)
        .await
        .unwrap(),
        0,
        "refund failure must roll back the spool insert"
    );
    assert_eq!(
        budget(&db).await,
        0,
        "refund failure must roll back admission"
    );
    sqlx::query("DROP TRIGGER reject_compressed_refund")
        .execute(&db.pool)
        .await
        .unwrap();

    let prepared = archive.prepare_first_batch().await.unwrap();
    let (mut tx, now) = db.spool_transaction().await.unwrap();
    assert!(
        db.capture_buffered_archive_body_in_transaction(&mut tx, now, &archive, Some(prepared))
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();

    let row = sqlx::query(
        "SELECT s.byte_count, s.cipher_bytes, b.cipher_bytes AS budget_bytes, b.request_cipher_bytes, 1024 + SUM(LENGTH(c.ciphertext) + 512) AS actual_bytes, MIN(CASE WHEN c.ciphertext LIKE 'zstd1.%' THEN 1 ELSE 0 END) AS compressed FROM request_archive_spools s JOIN request_archive_spool_chunks c ON c.request_id = s.request_id CROSS JOIN response_archive_spool_budget b WHERE s.request_id = $1 AND b.singleton = 1 GROUP BY s.byte_count, s.cipher_bytes, b.cipher_bytes, b.request_cipher_bytes",
    )
    .bind(id.request_id.to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let actual = row.get::<i64, _>("actual_bytes");
    assert_eq!(row.get::<i64, _>("byte_count"), body.len() as i64);
    assert_eq!(row.get::<i64, _>("cipher_bytes"), actual);
    assert_eq!(row.get::<i64, _>("budget_bytes"), actual);
    assert_eq!(row.get::<i64, _>("request_cipher_bytes"), actual);
    assert_eq!(row.get::<i64, _>("compressed"), 1);
    assert!(
        actual < legacy_upper,
        "the conservative reservation must be refunded"
    );

    let (mut replay, replay_now) = db.spool_transaction().await.unwrap();
    assert!(
        db.capture_buffered_archive_body_in_transaction(&mut replay, replay_now, &archive, None)
            .await
            .unwrap()
    );
    replay.commit().await.unwrap();
    assert_eq!(
        budget(&db).await,
        actual,
        "exact replay cannot charge twice"
    );
}

#[tokio::test]
async fn bounded_multirow_capture_rolls_back_across_batch_boundary() {
    let (_dir, db, id) = fixture().await;
    let chunks: Vec<_> = (0..257)
        .map(|seq| ArchiveSpoolChunk {
            seq,
            byte_count: 1,
            ciphertext: "opaque".into(),
        })
        .collect();
    sqlx::query("CREATE TRIGGER reject_second_batch BEFORE INSERT ON request_archive_spool_chunks WHEN NEW.seq = 128 BEGIN SELECT RAISE(ABORT, 'second batch'); END").execute(&db.pool).await.unwrap();
    assert!(
        db.capture_buffered_archive_spool(id, BufferedArchivePurpose::Request, &chunks)
            .await
            .is_err()
    );
    let row = sqlx::query("SELECT cipher_bytes, request_cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1").fetch_one(&db.pool).await.unwrap();
    assert_eq!(row.get::<i64, _>("cipher_bytes"), 0);
    assert_eq!(row.get::<i64, _>("request_cipher_bytes"), 0);
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_archive_spool_chunks")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
    sqlx::query("DROP TRIGGER reject_second_batch")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        db.capture_buffered_archive_spool(id, BufferedArchivePurpose::Request, &chunks)
            .await
            .unwrap()
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_archive_spool_chunks")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 257);
}

#[tokio::test]
async fn v79_response_budget_mutations_preserve_v80_request_subcounter() {
    let (_dir, db, id) = fixture().await;
    let chunks = [ArchiveSpoolChunk {
        seq: 0,
        byte_count: 1,
        ciphertext: "opaque".into(),
    }];
    assert!(
        db.capture_buffered_archive_spool(id, BufferedArchivePurpose::Request, &chunks)
            .await
            .unwrap()
    );
    let request_bytes = budget(&db).await;
    // These streaming response methods execute the unchanged v79 SQL, which
    // knows only cipher_bytes and must coexist with request accounting.
    assert!(db.begin_response_archive_spool(id).await.unwrap());
    assert!(
        db.append_response_archive_spool(id, 0, 1, "opaque")
            .await
            .unwrap()
    );
    assert!(db.seal_response_archive_spool(id, 1, 1).await.unwrap());
    let row = sqlx::query("SELECT cipher_bytes, request_cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1").fetch_one(&db.pool).await.unwrap();
    assert_eq!(row.get::<i64, _>("cipher_bytes"), request_bytes * 2);
    assert_eq!(row.get::<i64, _>("request_cipher_bytes"), request_bytes);
    sqlx::query("UPDATE response_archive_spools SET expires_at = 0")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(db.cleanup_response_archive_spools(32).await.unwrap(), 1);
    let row = sqlx::query("SELECT cipher_bytes, request_cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1").fetch_one(&db.pool).await.unwrap();
    assert_eq!(row.get::<i64, _>("cipher_bytes"), request_bytes);
    assert_eq!(row.get::<i64, _>("request_cipher_bytes"), request_bytes);
    // Legacy workers cannot incorrectly erase request charges: the database
    // check fails closed even though old binaries do not know the subcounter.
    assert!(
        sqlx::query("UPDATE response_archive_spool_budget SET cipher_bytes = 0")
            .execute(&db.pool)
            .await
            .is_err()
    );
    sqlx::query("UPDATE request_archive_spools SET expires_at = 0")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(db.cleanup_response_archive_spools(32).await.unwrap(), 1);
    let row = sqlx::query("SELECT cipher_bytes, request_cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1").fetch_one(&db.pool).await.unwrap();
    assert_eq!(row.get::<i64, _>("cipher_bytes"), 0);
    assert_eq!(row.get::<i64, _>("request_cipher_bytes"), 0);
    let active: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_archive_spools WHERE cleaned_at IS NULL AND state IN ('capturing', 'pending', 'uploading')").fetch_one(&db.pool).await.unwrap();
    assert_eq!(active, 0);
}

#[tokio::test]
async fn durable_admission_rolls_back_reservation_record_event_and_spool_together() {
    use crate::db::{CreateKeyInput, StartProxyRequest};
    let (_dir, db, _) = fixture().await;
    let pepper = b"durable-admission-test-pepper-over-32-bytes";
    let issued = db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "durable-admission".into(),
                principal_external_id: "member".into(),
                alias: "durable-admission".into(),
                currency: "USD".into(),
                policy: crate::model::KeyPolicy::default(),
                initial_balance: rust_decimal::Decimal::ONE,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let key = db.authenticate_key(&issued.key, pepper).await.unwrap();
    let price = db
        .upsert_model_price(
            "durable-admission",
            "USD",
            rust_decimal::Decimal::ONE,
            rust_decimal::Decimal::ONE,
        )
        .await
        .unwrap();
    let request_id = Uuid::new_v4();
    let locator = format!("gap://{request_id}/request");
    let input = || StartProxyRequest {
        request_id,
        key: &key,
        price: &price,
        input_token_ceiling: 7,
        output_token_ceiling: 11,
        protocol: "openai",
        model: "durable-admission",
        request_object: &locator,
        upstream_account_id: None,
        model_route_id: None,
    };
    let body = bytes::Bytes::from_static(b"{\"private\":\"request\"}");
    sqlx::query("CREATE TRIGGER reject_admission_chunk BEFORE INSERT ON request_archive_spool_chunks BEGIN SELECT RAISE(ABORT, 'test'); END").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.start_proxy_request_with_archive(input(), &body, pepper)
            .await,
        Err(AppError::Overloaded)
    ));
    let reservations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM usage_reservations WHERE key_id = $1")
            .bind(key.key_id.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(reservations, 0);
    for table in [
        "request_records",
        "request_record_locators",
        "request_events",
    ] {
        let column = if table == "request_events" {
            "request_id"
        } else {
            "id"
        };
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM {table} WHERE {column} = $1"
        )))
        .bind(request_id.to_string())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(count, 0);
    }
    assert_eq!(budget(&db).await, 0);
    sqlx::query("DROP TRIGGER reject_admission_chunk")
        .execute(&db.pool)
        .await
        .unwrap();
    let reservation = db
        .start_proxy_request_with_archive(input(), &body, pepper)
        .await
        .unwrap();
    let row = sqlx::query("SELECT s.state, s.reservation_id, r.completed_at FROM request_archive_spools s JOIN request_records r ON r.id = s.request_id WHERE s.request_id = $1").bind(request_id.to_string()).fetch_one(&db.pool).await.unwrap();
    assert_eq!(row.get::<String, _>("state"), "pending");
    assert_eq!(
        row.get::<String, _>("reservation_id"),
        reservation.id.to_string()
    );
    assert_eq!(row.get::<Option<i64>, _>("completed_at"), None);
    assert!(
        db.claim_archive_spool_if(Uuid::new_v4(), BufferedArchivePurpose::Request, || true)
            .await
            .unwrap()
            .is_none()
    );
}
#[tokio::test]
async fn request_binding_is_atomic_with_staging_and_preserves_terminal_facts() {
    use crate::archive_staging::{
        ArchiveStagingIntentDigest, ArchiveStagingKey, ArchiveStagingLeaseOwner,
        ArchiveStagingState, BeginArchiveStagingInput, BeginArchiveStagingResult,
    };
    let (_dir, db, id) = fixture().await;
    assert!(
        db.capture_buffered_archive_spool(
            id,
            BufferedArchivePurpose::Request,
            &[ArchiveSpoolChunk {
                seq: 0,
                byte_count: 3,
                ciphertext: "opaque".into()
            }]
        )
        .await
        .unwrap()
    );
    sqlx::query("UPDATE request_records SET completed_at = 2, request_object = $1, response_object = 'inline-json:{}'").bind(format!("gap://{}/request", id.request_id)).execute(&db.pool).await.unwrap();
    sqlx::query("UPDATE request_records SET request_object = $1")
        .bind(format!("gap://{}/request", id.request_id))
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE request_records SET status_code = 200, cost_micros = 123, input_tokens = 45, output_tokens = 67").execute(&db.pool).await.unwrap();
    let task = db
        .claim_archive_spool_if(Uuid::new_v4(), BufferedArchivePurpose::Request, || true)
        .await
        .unwrap()
        .unwrap();
    let key = ArchiveStagingKey::new(
        ArchiveStagingOwner::ProxyRequest(id.request_id),
        ArchiveStagingPurpose::Request,
        Uuid::new_v4(),
    )
    .unwrap();
    let lease = match db
        .begin_archive_staging_attempt(BeginArchiveStagingInput {
            key,
            intent_digest: ArchiveStagingIntentDigest::new("a".repeat(64)).unwrap(),
            lease_token: Uuid::new_v4(),
            lease_owner: ArchiveStagingLeaseOwner::new("spool-test").unwrap(),
        })
        .await
        .unwrap()
    {
        BeginArchiveStagingResult::Created(lease) => lease,
        _ => panic!("new staging attempt must be created"),
    };
    let locator = format!("{}/request.json", key.canonical_prefix());
    let mut bad_lease = lease.clone();
    bad_lease.token = Uuid::new_v4();
    assert!(
        !db.complete_response_archive_spool(&task, &bad_lease, &locator)
            .await
            .unwrap()
    );
    let current: String =
        sqlx::query_scalar("SELECT request_object FROM request_records WHERE id = $1")
            .bind(id.request_id.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(current, format!("gap://{}/request", id.request_id));
    // An already-replaced request locator cannot be overwritten.
    sqlx::query("UPDATE request_records SET request_object = 'preserved/request.json'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        !db.complete_response_archive_spool(&task, &lease, &locator)
            .await
            .unwrap()
    );
    assert_eq!(
        db.archive_staging_attempt(key.attempt_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ArchiveStagingState::Writing
    );
    sqlx::query("UPDATE request_records SET completed_at = 2, request_object = $1, response_object = 'inline-json:{}'").bind(format!("gap://{}/request", id.request_id)).execute(&db.pool).await.unwrap();
    assert!(
        db.complete_response_archive_spool(&task, &lease, &locator)
            .await
            .unwrap()
    );
    let events = db.all_request_events_after(0, None, 10).await.unwrap();
    let bound_events = events
        .iter()
        .filter(|event| event.event_kind == "archive_bound")
        .collect::<Vec<_>>();
    assert_eq!(bound_events.len(), 1);
    assert_eq!(
        bound_events[0].archive_state,
        crate::model::RequestArchiveState::Bound
    );
    assert_eq!(bound_events[0].input_tokens, 45);
    assert_eq!(bound_events[0].output_tokens, 67);
    assert_eq!(bound_events[0].billing.cost.as_deref(), Some("0.000123"));
    // Simulate lost completion ACK and worker error handling: neither retry
    // nor producer failure may release the bound object or change its locator.
    db.retry_response_archive_spool(&task, "upload_failed")
        .await
        .unwrap();
    db.fail_response_archive_spool(id, "capture_failed")
        .await
        .unwrap();
    let bound_event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_events WHERE request_id = $1 AND event_kind = 'archive_bound'",
    )
    .bind(id.request_id.to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(bound_event_count, 1);
    let row = sqlx::query("SELECT request_object, status_code, cost_micros, input_tokens, output_tokens, completed_at FROM request_records WHERE id = $1")
        .bind(id.request_id.to_string()).fetch_one(&db.pool).await.unwrap();
    assert_eq!(row.get::<String, _>("request_object"), locator);
    assert_eq!(row.get::<i64, _>("status_code"), 200);
    assert_eq!(row.get::<i64, _>("cost_micros"), 123);
    assert_eq!(row.get::<i64, _>("input_tokens"), 45);
    assert_eq!(row.get::<i64, _>("output_tokens"), 67);
    assert_eq!(row.get::<i64, _>("completed_at"), 2);
    assert_eq!(
        db.archive_staging_attempt(key.attempt_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ArchiveStagingState::Bound
    );
    assert_eq!(db.cleanup_response_archive_spools(32).await.unwrap(), 1);
    assert_eq!(budget(&db).await, 0);
    assert_eq!(
        db.archive_staging_attempt(key.attempt_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ArchiveStagingState::Bound
    );
    assert!(
        !db.complete_response_archive_spool(&task, &lease, &locator)
            .await
            .unwrap()
    );
}

#[path = "gc_tests.rs"]
mod gc_tests;
#[path = "postgres_tests.rs"]
mod postgres_tests;

#[tokio::test]
async fn buffered_atomic_capture_shares_budget_and_rolls_back_all_chunks() {
    let (_dir, db, id) = fixture().await;
    let chunks = vec![ArchiveSpoolChunk {
        seq: 0,
        byte_count: 3,
        ciphertext: "opaque".into(),
    }];
    sqlx::query("CREATE TRIGGER reject_request_chunk BEFORE INSERT ON request_archive_spool_chunks BEGIN SELECT RAISE(ABORT, 'test'); END").execute(&db.pool).await.unwrap();
    assert!(
        db.capture_buffered_archive_spool(id, BufferedArchivePurpose::Request, &chunks)
            .await
            .is_err()
    );
    assert_eq!(budget(&db).await, 0);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_archive_spools")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    sqlx::query("DROP TRIGGER reject_request_chunk")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        db.capture_buffered_archive_spool(id, BufferedArchivePurpose::Request, &chunks)
            .await
            .unwrap()
    );
    let request_budget = budget(&db).await;
    // Exact lost-ACK replay neither duplicates chunks nor charges admission.
    assert!(
        db.capture_buffered_archive_spool(id, BufferedArchivePurpose::Request, &chunks)
            .await
            .unwrap()
    );
    assert_eq!(budget(&db).await, request_budget);
    sqlx::query("UPDATE response_archive_spool_budget SET cipher_bytes = $1")
        .bind(CIPHER_LIMIT)
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        !db.capture_buffered_archive_spool(id, BufferedArchivePurpose::Response, &chunks)
            .await
            .unwrap()
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM response_archive_spools")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    sqlx::query("UPDATE response_archive_spool_budget SET cipher_bytes = $1")
        .bind(request_budget)
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        db.capture_buffered_archive_spool(id, BufferedArchivePurpose::Response, &chunks)
            .await
            .unwrap()
    );
    assert_eq!(budget(&db).await, request_budget * 2);
    sqlx::query("UPDATE request_archive_spools SET expires_at = 0")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(db.cleanup_response_archive_spools(32).await.unwrap(), 1);
    assert_eq!(budget(&db).await, request_budget);
    let state: String = sqlx::query_scalar("SELECT state FROM response_archive_spools")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(state, "pending");
}

#[tokio::test]
async fn request_spool_restart_terminal_gate_and_ha_fencing() {
    let (dir, db, id) = fixture().await;
    let chunks = vec![ArchiveSpoolChunk {
        seq: 0,
        byte_count: 3,
        ciphertext: "opaque".into(),
    }];
    assert!(
        db.capture_buffered_archive_spool(id, BufferedArchivePurpose::Request, &chunks)
            .await
            .unwrap()
    );
    assert!(
        db.claim_archive_spool_if(Uuid::new_v4(), BufferedArchivePurpose::Request, || true)
            .await
            .unwrap()
            .is_none()
    );
    terminal(&db, id).await;
    sqlx::query("UPDATE request_records SET request_object = $1")
        .bind(format!("gap://{}/request", id.request_id))
        .execute(&db.pool)
        .await
        .unwrap();
    let restarted = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("spool.db").display()
    ))
    .await
    .unwrap();
    let task = restarted
        .claim_archive_spool_if(Uuid::new_v4(), BufferedArchivePurpose::Request, || true)
        .await
        .unwrap()
        .unwrap();
    assert!(
        db.claim_archive_spool_if(Uuid::new_v4(), BufferedArchivePurpose::Request, || true)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        db.load_response_archive_spool_batch(&task, 0)
            .await
            .unwrap()
            .len(),
        1
    );
    sqlx::query("UPDATE request_archive_spools SET lease_expires_at = 0")
        .execute(&db.pool)
        .await
        .unwrap();
    let replacement = db
        .claim_archive_spool_if(Uuid::new_v4(), BufferedArchivePurpose::Request, || true)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(replacement.lease_token, task.lease_token);
    assert!(!db.heartbeat_response_archive_spool(&task).await.unwrap());
    assert!(
        db.load_response_archive_spool_batch(&task, 0)
            .await
            .unwrap()
            .is_empty()
    );
    db.retry_response_archive_spool(&task, "upload_failed")
        .await
        .unwrap();
    assert!(
        db.heartbeat_response_archive_spool(&replacement)
            .await
            .unwrap()
    );
}

async fn fixture() -> (tempfile::TempDir, Database, ArchiveSpoolIdentity) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("spool.db").display()
    ))
    .await
    .unwrap();
    db.migrate().await.unwrap();
    let id = ArchiveSpoolIdentity {
        request_id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
    };
    let key_id = Uuid::new_v4();
    sqlx::query("INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, input_tokens, output_tokens, cost_micros, request_object, reservation_id) VALUES ($1, $2, $3, 1, 'responses', 'test', 0, 0, 0, 'inline-json:{}', $4)")
        .bind(id.request_id.to_string()).bind(id.tenant_id.to_string()).bind(key_id.to_string()).bind(id.reservation_id.to_string()).execute(&db.pool).await.unwrap();
    sqlx::query("INSERT INTO request_record_locators (id, created_at, tenant_id, key_id) VALUES ($1, 1, $2, $3)")
        .bind(id.request_id.to_string()).bind(id.tenant_id.to_string()).bind(key_id.to_string())
        .execute(&db.pool).await.unwrap();
    (dir, db, id)
}

async fn budget(db: &Database) -> i64 {
    sqlx::query_scalar("SELECT cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1")
        .fetch_one(&db.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn terminal_gap_event_is_atomic_idempotent_and_preserves_snapshot_facts() {
    let (_dir, db, id) = fixture().await;
    assert!(db.begin_response_archive_spool(id).await.unwrap());
    terminal(&db, id).await;
    sqlx::query("UPDATE request_records SET status_code = 503, duration_ms = 9, input_tokens = 45, output_tokens = 67, cost_micros = 123, currency = 'USD', error_code = 'upstream_error' WHERE id = $1")
        .bind(id.request_id.to_string())
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER abort_archive_gap_event BEFORE INSERT ON request_events WHEN NEW.event_kind = 'archive_gap' BEGIN SELECT RAISE(ABORT, 'fixture event failure'); END",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    assert!(
        db.fail_response_archive_spool(id, "upload_failed")
            .await
            .is_err()
    );
    let state: String =
        sqlx::query_scalar("SELECT state FROM response_archive_spools WHERE request_id = $1")
            .bind(id.request_id.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(state, "capturing");
    let event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_events WHERE request_id = $1 AND event_kind = 'archive_gap'",
    )
    .bind(id.request_id.to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(event_count, 0);

    sqlx::query("DROP TRIGGER abort_archive_gap_event")
        .execute(&db.pool)
        .await
        .unwrap();
    db.fail_response_archive_spool(id, "upload_failed")
        .await
        .unwrap();
    db.fail_response_archive_spool(id, "capture_failed")
        .await
        .unwrap();
    let events = db.all_request_events_after(0, None, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.request_id, id.request_id);
    assert_eq!(event.event_kind, "archive_gap");
    assert_eq!(event.archive_state, crate::model::RequestArchiveState::Gap);
    assert_eq!(event.status_code, Some(503));
    assert_eq!(event.input_tokens, 45);
    assert_eq!(event.output_tokens, 67);
    assert_eq!(event.billing.cost.as_deref(), Some("0.000123"));
    assert_eq!(event.billing.currency.as_deref(), Some("USD"));
    assert_eq!(event.error_code.as_deref(), Some("upstream_error"));
}

async fn terminal(db: &Database, id: ArchiveSpoolIdentity) {
    sqlx::query("UPDATE request_records SET completed_at = 2, response_object = $1 WHERE id = $2")
        .bind(format!("gap://{}/response", id.request_id))
        .bind(id.request_id.to_string())
        .execute(&db.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn identity_sequence_replay_and_exact_quota() {
    let (_dir, db, id) = fixture().await;
    let alien = ArchiveSpoolIdentity {
        tenant_id: Uuid::new_v4(),
        ..id
    };
    sqlx::query(
        "UPDATE request_records SET completed_at = 2, response_object = 'archive/already-bound' WHERE id = $1",
    )
    .bind(id.request_id.to_string())
    .execute(&db.pool)
    .await
    .unwrap();
    assert!(!db.begin_response_archive_spool(id).await.unwrap());
    terminal(&db, id).await;
    assert!(!db.begin_response_archive_spool(alien).await.unwrap());
    assert!(db.begin_response_archive_spool(id).await.unwrap());
    assert!(db.begin_response_archive_spool(id).await.unwrap());
    assert!(
        !db.append_response_archive_spool(alien, 0, 8, "cipher")
            .await
            .unwrap()
    );
    assert!(
        !db.append_response_archive_spool(id, 1, 8, "cipher")
            .await
            .unwrap()
    );
    assert!(
        db.append_response_archive_spool(id, 0, 8, "cipher")
            .await
            .unwrap()
    );
    assert!(
        db.append_response_archive_spool(id, 0, 8, "cipher")
            .await
            .unwrap()
    );
    assert!(
        !db.append_response_archive_spool(id, 0, 9, "cipher")
            .await
            .unwrap()
    );
    assert!(
        !db.append_response_archive_spool(id, 0, 8, "other")
            .await
            .unwrap()
    );
    assert_eq!(budget(&db).await, SPOOL_OVERHEAD + 6 + CHUNK_OVERHEAD);
    assert!(
        !db.append_response_archive_spool(id, 1, 8, &"x".repeat(CIPHER_CHUNK_LIMIT + 1))
            .await
            .unwrap()
    );
    // Seed the global accounting boundary without allocating 256MiB in CI.
    sqlx::query("UPDATE response_archive_spool_budget SET cipher_bytes = $1")
        .bind(CIPHER_LIMIT - 3 - CHUNK_OVERHEAD)
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        db.append_response_archive_spool(id, 1, 8, "abc")
            .await
            .unwrap()
    );
    assert_eq!(budget(&db).await, CIPHER_LIMIT);
    assert!(
        !db.append_response_archive_spool(id, 2, 8, "x")
            .await
            .unwrap()
    );
    assert_eq!(budget(&db).await, CIPHER_LIMIT);
    assert!(!db.seal_response_archive_spool(id, 2, 15).await.unwrap());
    assert!(db.seal_response_archive_spool(id, 2, 16).await.unwrap());
    assert!(db.seal_response_archive_spool(id, 2, 16).await.unwrap());
    assert!(
        !db.append_response_archive_spool(id, 2, 1, "x")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn gap_cleanup_immediately_releases_exact_cipher_bytes_and_preserves_audit() {
    let (_dir, db, id) = fixture().await;
    assert!(db.begin_response_archive_spool(id).await.unwrap());
    assert!(
        db.append_response_archive_spool(id, 0, PLAIN_LIMIT, "envelope")
            .await
            .unwrap()
    );
    assert!(
        !db.append_response_archive_spool(id, 1, 1, "x")
            .await
            .unwrap()
    );
    assert_eq!(budget(&db).await, SPOOL_OVERHEAD + 8 + CHUNK_OVERHEAD);
    db.fail_response_archive_spool(id, "payload-bearing-untrusted-reason")
        .await
        .unwrap();
    let row = sqlx::query(
        "SELECT state, last_error_code, updated_at, expires_at FROM response_archive_spools",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("state"), "gap");
    assert_eq!(row.get::<String, _>("last_error_code"), "internal");
    assert_eq!(
        row.get::<i64, _>("expires_at"),
        row.get::<i64, _>("updated_at")
    );
    assert_eq!(db.cleanup_response_archive_spools(0).await.unwrap(), 0);
    assert_eq!(db.cleanup_response_archive_spools(1).await.unwrap(), 1);
    assert_eq!(budget(&db).await, 0);
    assert_eq!(db.cleanup_response_archive_spools(32).await.unwrap(), 0);
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_records")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 1);
    let audit: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM response_archive_spools WHERE state = 'gap' AND cleaned_at IS NOT NULL AND byte_count = $1").bind(PLAIN_LIMIT).fetch_one(&db.pool).await.unwrap();
    assert_eq!(audit, 1);
}

#[tokio::test]
async fn exhausted_upload_gap_is_immediately_cleanup_eligible() {
    let (_dir, db, id) = fixture().await;
    assert!(db.begin_response_archive_spool(id).await.unwrap());
    assert!(
        db.append_response_archive_spool(id, 0, 1, "ciphertext")
            .await
            .unwrap()
    );
    assert!(db.seal_response_archive_spool(id, 1, 1).await.unwrap());
    terminal(&db, id).await;
    let task = db
        .claim_response_archive_spool(Uuid::new_v4())
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE response_archive_spools SET attempts = 10")
        .execute(&db.pool)
        .await
        .unwrap();

    db.retry_response_archive_spool(&task, "upload_failed")
        .await
        .unwrap();
    let row = sqlx::query(
        "SELECT state, last_error_code, updated_at, expires_at FROM response_archive_spools",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("state"), "gap");
    assert_eq!(row.get::<String, _>("last_error_code"), "upload_failed");
    assert_eq!(
        row.get::<i64, _>("expires_at"),
        row.get::<i64, _>("updated_at")
    );
    assert_eq!(db.cleanup_response_archive_spools(1).await.unwrap(), 1);
    assert_eq!(budget(&db).await, 0);
    let audit: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM response_archive_spools WHERE state = 'gap' AND cleaned_at IS NOT NULL AND last_error_code = 'upload_failed'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(audit, 1);
}

#[tokio::test]
async fn cleanup_time_budget_stops_between_committed_batches() {
    let (_dir, db, first) = fixture().await;
    let second = ArchiveSpoolIdentity {
        request_id: Uuid::new_v4(),
        ..first
    };
    sqlx::query("INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, input_tokens, output_tokens, cost_micros, request_object, reservation_id) VALUES ($1, $2, $3, 1, 'responses', 'test', 0, 0, 0, 'gap://test/request', $4)")
        .bind(second.request_id.to_string()).bind(second.tenant_id.to_string())
        .bind(Uuid::new_v4().to_string()).bind(second.reservation_id.to_string())
        .execute(&db.pool).await.unwrap();
    for identity in [first, second] {
        assert!(db.begin_response_archive_spool(identity).await.unwrap());
        db.fail_response_archive_spool(identity, "capture_failed")
            .await
            .unwrap();
    }
    sqlx::query("UPDATE response_archive_spools SET expires_at = 0")
        .execute(&db.pool)
        .await
        .unwrap();

    assert_eq!(
        db.cleanup_response_archive_spools_for(32, Duration::ZERO)
            .await
            .unwrap(),
        1
    );
    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM response_archive_spools WHERE cleaned_at IS NULL")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(remaining, 1);
    assert_eq!(db.cleanup_response_archive_spools(32).await.unwrap(), 1);
    assert_eq!(budget(&db).await, 0);
}

#[tokio::test]
async fn sqlite_cleanup_preserves_oldest_eligible_fairness_across_classes() {
    let (_dir, db, identity) = fixture().await;
    let expired = identity.request_id.to_string();
    let bound = Uuid::new_v4().to_string();
    let exhausted = Uuid::new_v4().to_string();
    for (request_id, state, updated_at, expires_at, attempts, lease_expires_at) in [
        (&expired, "gap", -30_i64, -30_i64, 0_i64, None),
        (&bound, "bound", -20, i64::MAX, 0, None),
        (&exhausted, "uploading", -10, i64::MAX, 10, Some(-10_i64)),
    ] {
        sqlx::query("INSERT INTO response_archive_spools (request_id, tenant_id, reservation_id, state, cipher_bytes, attempts, next_attempt_at, created_at, updated_at, expires_at, lease_expires_at) VALUES ($1, $2, $3, $4, $5, $6, 0, 0, $7, $8, $9)")
            .bind(request_id)
            .bind(identity.tenant_id.to_string())
            .bind(identity.reservation_id.to_string())
            .bind(state)
            .bind(SPOOL_OVERHEAD)
            .bind(attempts)
            .bind(updated_at)
            .bind(expires_at)
            .bind(lease_expires_at)
            .execute(&db.pool)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE response_archive_spool_budget SET cipher_bytes = $1")
        .bind(3 * SPOOL_OVERHEAD)
        .execute(&db.pool)
        .await
        .unwrap();

    for expected in [&expired, &bound, &exhausted] {
        assert_eq!(db.cleanup_response_archive_spools(1).await.unwrap(), 1);
        let cleaned: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM response_archive_spools WHERE request_id = $1 AND cleaned_at IS NOT NULL",
        )
        .bind(expected)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(cleaned, 1, "oldest eligible class must win");
    }
    assert_eq!(budget(&db).await, 0);
}

#[tokio::test]
async fn sqlite_concurrent_cleanup_workers_serialize_and_both_make_progress() {
    let (_dir, db, first) = fixture().await;
    let second = ArchiveSpoolIdentity {
        request_id: Uuid::new_v4(),
        ..first
    };
    sqlx::query("INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, input_tokens, output_tokens, cost_micros, request_object, reservation_id) VALUES ($1, $2, $3, 1, 'responses', 'test', 0, 0, 0, 'gap://test/request', $4)")
        .bind(second.request_id.to_string()).bind(second.tenant_id.to_string())
        .bind(Uuid::new_v4().to_string()).bind(second.reservation_id.to_string())
        .execute(&db.pool).await.unwrap();
    for identity in [first, second] {
        assert!(db.begin_response_archive_spool(identity).await.unwrap());
    }
    sqlx::query("UPDATE response_archive_spools SET expires_at = 0")
        .execute(&db.pool)
        .await
        .unwrap();

    let (left, right) = tokio::join!(
        db.cleanup_response_archive_spools(1),
        db.cleanup_response_archive_spools(1)
    );
    assert_eq!(left.unwrap(), 1);
    assert_eq!(right.unwrap(), 1);
    assert_eq!(budget(&db).await, 0);
}

#[tokio::test]
async fn claims_require_terminal_gap_and_fence_old_leases() {
    let (_dir, db, id) = fixture().await;
    assert!(db.begin_response_archive_spool(id).await.unwrap());
    assert!(
        db.append_response_archive_spool(id, 0, 3, "opaque")
            .await
            .unwrap()
    );
    let owner = Uuid::new_v4();
    assert!(
        db.claim_response_archive_spool(owner)
            .await
            .unwrap()
            .is_none()
    );
    assert!(db.seal_response_archive_spool(id, 1, 3).await.unwrap());
    // Simulate a seal commit whose ACK was lost: producer failure cleanup
    // must leave the now-complete pending row recoverable.
    db.fail_response_archive_spool(id, "capture_timeout")
        .await
        .unwrap();
    assert!(
        db.claim_response_archive_spool(owner)
            .await
            .unwrap()
            .is_none()
    );
    terminal(&db, id).await;
    let first = db
        .claim_response_archive_spool(owner)
        .await
        .unwrap()
        .unwrap();
    assert!(
        db.claim_response_archive_spool(owner)
            .await
            .unwrap()
            .is_none()
    );
    let chunk = db
        .load_response_archive_spool_chunk(&first, 0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(chunk.seq, 0);
    assert_eq!(chunk.byte_count, 3);
    assert_eq!(chunk.ciphertext, "opaque");
    assert!(db.heartbeat_response_archive_spool(&first).await.unwrap());
    let mut forged = first.clone();
    forged.identity.tenant_id = Uuid::new_v4();
    assert!(
        db.load_response_archive_spool_chunk(&forged, 0)
            .await
            .unwrap()
            .is_none()
    );
    forged = first.clone();
    forged.lease_token = Uuid::new_v4();
    assert!(!db.heartbeat_response_archive_spool(&forged).await.unwrap());
    sqlx::query("UPDATE response_archive_spools SET lease_expires_at = 0")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(!db.heartbeat_response_archive_spool(&first).await.unwrap());
    let second = db
        .claim_response_archive_spool(owner)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(first.lease_token, second.lease_token);
    db.retry_response_archive_spool(&first, "upload_failed")
        .await
        .unwrap();
    assert!(db.heartbeat_response_archive_spool(&second).await.unwrap());
    db.retry_response_archive_spool(&second, "upload_failed")
        .await
        .unwrap();
    assert!(
        db.claim_response_archive_spool(owner)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!db.heartbeat_response_archive_spool(&second).await.unwrap());
    sqlx::query("UPDATE response_archive_spools SET expires_at = 0")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(db.cleanup_response_archive_spools(32).await.unwrap(), 1);
    assert_eq!(budget(&db).await, 0);
}

#[tokio::test]
async fn stale_capture_is_reclaimed() {
    let (_dir, db, id) = fixture().await;
    assert!(db.begin_response_archive_spool(id).await.unwrap());
    assert!(
        db.append_response_archive_spool(id, 0, 1, "x")
            .await
            .unwrap()
    );
    sqlx::query("UPDATE response_archive_spools SET expires_at = 0")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(!db.begin_response_archive_spool(id).await.unwrap());
    assert!(!db.seal_response_archive_spool(id, 1, 1).await.unwrap());
    assert_eq!(db.cleanup_response_archive_spools(32).await.unwrap(), 1);
    assert_eq!(budget(&db).await, 0);
    assert!(!db.begin_response_archive_spool(id).await.unwrap());
}

#[tokio::test]
async fn binding_is_atomic_with_staging_and_preserves_terminal_facts() {
    use crate::archive_staging::{
        ArchiveStagingIntentDigest, ArchiveStagingKey, ArchiveStagingLeaseOwner,
        ArchiveStagingState, BeginArchiveStagingInput, BeginArchiveStagingResult,
    };
    let (_dir, db, id) = fixture().await;
    assert!(db.begin_response_archive_spool(id).await.unwrap());
    assert!(
        db.append_response_archive_spool(id, 0, 3, "opaque")
            .await
            .unwrap()
    );
    assert!(db.seal_response_archive_spool(id, 1, 3).await.unwrap());
    terminal(&db, id).await;
    sqlx::query("UPDATE request_records SET status_code = 200, cost_micros = 123, input_tokens = 45, output_tokens = 67").execute(&db.pool).await.unwrap();
    let task = db
        .claim_response_archive_spool(Uuid::new_v4())
        .await
        .unwrap()
        .unwrap();
    let key = ArchiveStagingKey::new(
        ArchiveStagingOwner::ProxyRequest(id.request_id),
        ArchiveStagingPurpose::Response,
        Uuid::new_v4(),
    )
    .unwrap();
    let lease = match db
        .begin_archive_staging_attempt(BeginArchiveStagingInput {
            key,
            intent_digest: ArchiveStagingIntentDigest::new("a".repeat(64)).unwrap(),
            lease_token: Uuid::new_v4(),
            lease_owner: ArchiveStagingLeaseOwner::new("spool-test").unwrap(),
        })
        .await
        .unwrap()
    {
        BeginArchiveStagingResult::Created(lease) => lease,
        _ => panic!("new staging attempt must be created"),
    };
    let locator = format!("{}/response.json", key.canonical_prefix());
    let mut bad_lease = lease.clone();
    bad_lease.token = Uuid::new_v4();
    assert!(
        !db.complete_response_archive_spool(&task, &bad_lease, &locator)
            .await
            .unwrap()
    );
    let current: String =
        sqlx::query_scalar("SELECT response_object FROM request_records WHERE id = $1")
            .bind(id.request_id.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(current, format!("gap://{}/response", id.request_id));
    // An already-replaced request locator cannot be overwritten.
    sqlx::query("UPDATE request_records SET response_object = 'preserved/response.json'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        !db.complete_response_archive_spool(&task, &lease, &locator)
            .await
            .unwrap()
    );
    assert_eq!(
        db.archive_staging_attempt(key.attempt_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ArchiveStagingState::Writing
    );
    terminal(&db, id).await;
    assert!(
        db.complete_response_archive_spool(&task, &lease, &locator)
            .await
            .unwrap()
    );
    let events = db.all_request_events_after(0, None, 10).await.unwrap();
    let bound_events = events
        .iter()
        .filter(|event| event.event_kind == "archive_bound")
        .collect::<Vec<_>>();
    assert_eq!(bound_events.len(), 1);
    assert_eq!(
        bound_events[0].archive_state,
        crate::model::RequestArchiveState::Bound
    );
    assert_eq!(bound_events[0].input_tokens, 45);
    assert_eq!(bound_events[0].output_tokens, 67);
    assert_eq!(bound_events[0].billing.cost.as_deref(), Some("0.000123"));
    // Simulate lost completion ACK and worker error handling: neither retry
    // nor producer failure may release the bound object or change its locator.
    db.retry_response_archive_spool(&task, "upload_failed")
        .await
        .unwrap();
    db.fail_response_archive_spool(id, "capture_failed")
        .await
        .unwrap();
    let bound_event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_events WHERE request_id = $1 AND event_kind = 'archive_bound'",
    )
    .bind(id.request_id.to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(bound_event_count, 1);
    let row = sqlx::query("SELECT response_object, status_code, cost_micros, input_tokens, output_tokens, completed_at FROM request_records WHERE id = $1")
        .bind(id.request_id.to_string()).fetch_one(&db.pool).await.unwrap();
    assert_eq!(row.get::<String, _>("response_object"), locator);
    assert_eq!(row.get::<i64, _>("status_code"), 200);
    assert_eq!(row.get::<i64, _>("cost_micros"), 123);
    assert_eq!(row.get::<i64, _>("input_tokens"), 45);
    assert_eq!(row.get::<i64, _>("output_tokens"), 67);
    assert_eq!(row.get::<i64, _>("completed_at"), 2);
    assert_eq!(
        db.archive_staging_attempt(key.attempt_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ArchiveStagingState::Bound
    );
    assert_eq!(db.cleanup_response_archive_spools(32).await.unwrap(), 1);
    assert_eq!(budget(&db).await, 0);
    assert_eq!(
        db.archive_staging_attempt(key.attempt_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ArchiveStagingState::Bound
    );
    assert!(
        !db.complete_response_archive_spool(&task, &lease, &locator)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn empty_spool_admission_is_bounded_and_replay_does_not_recharge() {
    let (_dir, db, id) = fixture().await;
    sqlx::query("UPDATE response_archive_spool_budget SET cipher_bytes = $1")
        .bind(CIPHER_LIMIT - SPOOL_OVERHEAD + 1)
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(!db.begin_response_archive_spool(id).await.unwrap());
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM response_archive_spools")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
    assert_eq!(budget(&db).await, CIPHER_LIMIT - SPOOL_OVERHEAD + 1);
    sqlx::query("UPDATE response_archive_spool_budget SET cipher_bytes = $1")
        .bind(CIPHER_LIMIT - SPOOL_OVERHEAD)
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(db.begin_response_archive_spool(id).await.unwrap());
    assert_eq!(budget(&db).await, CIPHER_LIMIT);
    assert!(db.begin_response_archive_spool(id).await.unwrap());
    assert_eq!(budget(&db).await, CIPHER_LIMIT);
    assert!(
        !db.append_response_archive_spool(id, 0, 1, "x")
            .await
            .unwrap()
    );
    sqlx::query("UPDATE response_archive_spools SET expires_at = 0")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(db.cleanup_response_archive_spools(32).await.unwrap(), 1);
    assert_eq!(budget(&db).await, CIPHER_LIMIT - SPOOL_OVERHEAD);
    assert_eq!(db.cleanup_response_archive_spools(32).await.unwrap(), 0);
    assert!(!db.begin_response_archive_spool(id).await.unwrap());
}

#[tokio::test]
async fn competing_replays_charge_one_admission_and_one_chunk() {
    let (_dir, db, id) = fixture().await;
    let (left, right) = tokio::join!(
        db.begin_response_archive_spool(id),
        db.begin_response_archive_spool(id)
    );
    assert!(left.unwrap());
    assert!(right.unwrap());
    assert_eq!(budget(&db).await, SPOOL_OVERHEAD);
    let (left, right) = tokio::join!(
        db.append_response_archive_spool(id, 0, 1, "envelope"),
        db.append_response_archive_spool(id, 0, 1, "envelope")
    );
    assert!(left.unwrap());
    assert!(right.unwrap());
    assert_eq!(budget(&db).await, SPOOL_OVERHEAD + CHUNK_OVERHEAD + 8);
    assert!(db.seal_response_archive_spool(id, 1, 1).await.unwrap());
    terminal(&db, id).await;
    let (left, right) = tokio::join!(
        db.claim_response_archive_spool(Uuid::new_v4()),
        db.claim_response_archive_spool(Uuid::new_v4())
    );
    assert_eq!(
        usize::from(left.unwrap().is_some()) + usize::from(right.unwrap().is_some()),
        1
    );
}

#[tokio::test]
async fn exhausted_crash_lease_and_pending_retention_release_budget() {
    let (_dir, db, id) = fixture().await;
    assert!(db.begin_response_archive_spool(id).await.unwrap());
    assert!(
        db.append_response_archive_spool(id, 0, 1, "x")
            .await
            .unwrap()
    );
    assert!(db.seal_response_archive_spool(id, 1, 1).await.unwrap());
    let ttl: i64 =
        sqlx::query_scalar("SELECT expires_at - updated_at FROM response_archive_spools")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(ttl, RETENTION);
    terminal(&db, id).await;
    let task = db
        .claim_response_archive_spool(Uuid::new_v4())
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE response_archive_spools SET attempts = 10, lease_expires_at = 0")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        db.claim_response_archive_spool(Uuid::new_v4())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(db.cleanup_response_archive_spools(32).await.unwrap(), 1);
    assert_eq!(budget(&db).await, 0);
    assert!(
        db.load_response_archive_spool_chunk(&task, 0)
            .await
            .unwrap()
            .is_none()
    );
}
