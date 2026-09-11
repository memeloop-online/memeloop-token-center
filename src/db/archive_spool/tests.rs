use super::*;

#[path = "gc_tests.rs"]
mod gc_tests;
#[path = "postgres_tests.rs"]
mod postgres_tests;

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
    sqlx::query("INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, input_tokens, output_tokens, cost_micros, request_object, reservation_id) VALUES ($1, $2, $3, 1, 'responses', 'test', 0, 0, 0, 'gap://test/request', $4)")
        .bind(id.request_id.to_string()).bind(id.tenant_id.to_string()).bind(Uuid::new_v4().to_string()).bind(id.reservation_id.to_string()).execute(&db.pool).await.unwrap();
    (dir, db, id)
}

async fn budget(db: &Database) -> i64 {
    sqlx::query_scalar("SELECT cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1")
        .fetch_one(&db.pool)
        .await
        .unwrap()
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
async fn plaintext_budget_and_cleanup_release_exact_cipher_bytes() {
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
    let reason: String = sqlx::query_scalar("SELECT last_error_code FROM response_archive_spools")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(reason, "internal");
    assert_eq!(db.cleanup_response_archive_spools(32).await.unwrap(), 0);
    sqlx::query("UPDATE response_archive_spools SET expires_at = 0")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(db.cleanup_response_archive_spools(0).await.unwrap(), 0);
    assert_eq!(db.cleanup_response_archive_spools(100).await.unwrap(), 1);
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
    // Simulate lost completion ACK and worker error handling: neither retry
    // nor producer failure may release the bound object or change its locator.
    db.retry_response_archive_spool(&task, "upload_failed")
        .await
        .unwrap();
    db.fail_response_archive_spool(id, "capture_failed")
        .await
        .unwrap();
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
