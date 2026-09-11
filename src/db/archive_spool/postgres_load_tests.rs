use super::*;

#[tokio::test]
async fn postgres_gc_retries_contention_without_blocking_live_capture() {
    let Some(fixture) = PgFixture::new().await else {
        return;
    };
    assert!(
        fixture
            .db
            .begin_response_archive_spool(fixture.id)
            .await
            .unwrap()
    );
    for seq in 0..65 {
        assert!(
            fixture
                .db
                .append_response_archive_spool(fixture.id, seq, 1, "x")
                .await
                .unwrap()
        );
    }
    sqlx::query("UPDATE response_archive_spools SET expires_at = 0")
        .execute(&fixture.db.pool)
        .await
        .unwrap();
    let live = ArchiveSpoolIdentity {
        request_id: Uuid::new_v4(),
        ..fixture.id
    };
    sqlx::query("INSERT INTO request_records (id, tenant_id, reservation_id) VALUES ($1, $2, $3)")
        .bind(live.request_id.to_string())
        .bind(live.tenant_id.to_string())
        .bind(live.reservation_id.to_string())
        .execute(&fixture.db.pool)
        .await
        .unwrap();
    assert!(fixture.db.begin_response_archive_spool(live).await.unwrap());
    let db = fixture.db.clone();
    let producer = tokio::spawn(async move {
        for seq in 0..32 {
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(250),
                    db.append_response_archive_spool(live, seq, 1, "x")
                )
                .await
                .expect("producer ACK must stay bounded during small GC batches")
                .unwrap()
            );
            tokio::task::yield_now().await;
        }
    });
    for _ in 0..8 {
        // NOWAIT contention errors are allowed; blocking behind a producer
        // while holding a spool lock is not. Retry only bounded GC batches.
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            fixture.db.cleanup_response_archive_spools(1),
        )
        .await
        .expect("GC must complete or fail NOWAIT within bounded time");
        tokio::task::yield_now().await;
    }
    producer.await.unwrap();
    for _ in 0..3 {
        fixture.db.cleanup_response_archive_spools(1).await.unwrap();
    }
    let old_chunks: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM response_archive_spool_chunks WHERE request_id = $1",
    )
    .bind(fixture.id.request_id.to_string())
    .fetch_one(&fixture.db.pool)
    .await
    .unwrap();
    let live_chunks: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM response_archive_spool_chunks WHERE request_id = $1",
    )
    .bind(live.request_id.to_string())
    .fetch_one(&fixture.db.pool)
    .await
    .unwrap();
    assert_eq!(old_chunks, 0);
    assert_eq!(live_chunks, 32);
    assert_eq!(
        budget(&fixture.db).await,
        SPOOL_OVERHEAD + 32 * (CHUNK_OVERHEAD + 1)
    );
    fixture.finish().await;
}

#[tokio::test]
async fn postgres_large_tiny_chunk_and_audit_inventory_has_bounded_indexed_gc() {
    let Some(fixture) = PgFixture::new().await else {
        return;
    };
    // Eight legal spools, each below 65,536 chunks; ~448k tiny chunks consume
    // ~219MiB of the configured 256MiB accounting budget, including overhead.
    let count = 56_000_i64;
    let per_spool = SPOOL_OVERHEAD + count * (CHUNK_OVERHEAD + 1);
    let mut seed = fixture.db.pool.begin().await.unwrap();
    sqlx::query("SET LOCAL statement_timeout = 60000")
        .execute(&mut *seed)
        .await
        .unwrap();
    for _ in 0..8 {
        let request = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO request_records (id, tenant_id, reservation_id) VALUES ($1, $2, $3)",
        )
        .bind(&request)
        .bind(fixture.id.tenant_id.to_string())
        .bind(fixture.id.reservation_id.to_string())
        .execute(&mut *seed)
        .await
        .unwrap();
        sqlx::query("INSERT INTO response_archive_spools (request_id, tenant_id, reservation_id, state, chunk_count, byte_count, cipher_bytes, next_attempt_at, created_at, updated_at, expires_at) VALUES ($1, $2, $3, 'capturing', $4, $4, $5, 0, 0, 0, 0)")
            .bind(&request).bind(fixture.id.tenant_id.to_string()).bind(fixture.id.reservation_id.to_string())
            .bind(count).bind(per_spool).execute(&mut *seed).await.unwrap();
        sqlx::query("INSERT INTO response_archive_spool_chunks (request_id, seq, ciphertext, byte_count) SELECT $1, seq, 'x', 1 FROM generate_series(0::BIGINT, $2::BIGINT - 1) seq")
            .bind(&request).bind(count).execute(&mut *seed).await.unwrap();
    }
    sqlx::query("UPDATE response_archive_spool_budget SET cipher_bytes = $1")
        .bind(8 * per_spool)
        .execute(&mut *seed)
        .await
        .unwrap();
    // Historical audit rows are retained indefinitely but excluded from GC's
    // partial indexes. Generate the pressure fixture in SQL, not 448k RPCs.
    sqlx::query("INSERT INTO response_archive_spools (request_id, tenant_id, reservation_id, state, next_attempt_at, created_at, updated_at, expires_at, cleaned_at) SELECT md5('spool-audit-' || seq::TEXT)::UUID::TEXT, $1, $2, 'gap', 0, 0, 0, 0, 1 FROM generate_series(1, 448000) seq")
        .bind(fixture.id.tenant_id.to_string()).bind(fixture.id.reservation_id.to_string())
        .execute(&mut *seed).await.unwrap();
    sqlx::query("ANALYZE response_archive_spools")
        .execute(&mut *seed)
        .await
        .unwrap();
    sqlx::query("ANALYZE response_archive_spool_chunks")
        .execute(&mut *seed)
        .await
        .unwrap();
    seed.commit().await.unwrap();
    let plan = sqlx::query("EXPLAIN (FORMAT TEXT) SELECT request_id FROM response_archive_spools WHERE cleaned_at IS NULL AND expires_at <= 1 ORDER BY expires_at, request_id LIMIT 1")
        .fetch_all(&fixture.db.pool).await.unwrap().into_iter()
        .map(|row| row.get::<String, _>(0)).collect::<Vec<_>>().join("\n");
    assert!(plan.contains("response_archive_spool_expiry"), "{plan}");
    assert!(!plan.contains("Seq Scan"), "{plan}");
    for batch in 1..=3 {
        assert_eq!(
            fixture.db.cleanup_response_archive_spools(1).await.unwrap(),
            0
        );
        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM response_archive_spool_chunks")
                .fetch_one(&fixture.db.pool)
                .await
                .unwrap();
        assert_eq!(remaining, 8 * count - batch * GC_CHUNK_LIMIT);
        assert_eq!(
            budget(&fixture.db).await,
            8 * per_spool - batch * GC_CHUNK_LIMIT * (CHUNK_OVERHEAD + 1)
        );
    }
    // Bounded test: no requirement to synchronously drain all 448k rows.
    fixture.finish().await;
}

#[tokio::test]
async fn postgres_gc_nowait_rolls_back_deletes_when_producer_holds_budget() {
    let Some(fixture) = PgFixture::new().await else {
        return;
    };
    assert!(
        fixture
            .db
            .begin_response_archive_spool(fixture.id)
            .await
            .unwrap()
    );
    for seq in 0..65 {
        assert!(
            fixture
                .db
                .append_response_archive_spool(fixture.id, seq, 1, "x")
                .await
                .unwrap()
        );
    }
    sqlx::query("UPDATE response_archive_spools SET expires_at = 0")
        .execute(&fixture.db.pool)
        .await
        .unwrap();
    let before = budget(&fixture.db).await;
    let mut producer = fixture.db.pool.begin().await.unwrap();
    sqlx::query(
        "SELECT cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1 FOR UPDATE",
    )
    .fetch_one(&mut *producer)
    .await
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        fixture.db.cleanup_response_archive_spools(1),
    )
    .await
    .expect("GC must fail NOWAIT, never wait behind producer while holding spool");
    assert!(result.is_err());
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM response_archive_spool_chunks")
        .fetch_one(&fixture.db.pool)
        .await
        .unwrap();
    assert_eq!(remaining, 65);
    assert_eq!(budget(&fixture.db).await, before);
    let accounted: i64 = sqlx::query_scalar("SELECT cipher_bytes FROM response_archive_spools")
        .fetch_one(&fixture.db.pool)
        .await
        .unwrap();
    assert_eq!(accounted, before);
    producer.rollback().await.unwrap();
    assert_eq!(
        fixture.db.cleanup_response_archive_spools(1).await.unwrap(),
        0
    );
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM response_archive_spool_chunks")
        .fetch_one(&fixture.db.pool)
        .await
        .unwrap();
    assert_eq!(remaining, 1);
    assert_eq!(
        budget(&fixture.db).await,
        SPOOL_OVERHEAD + CHUNK_OVERHEAD + 1
    );
    assert_eq!(
        fixture.db.cleanup_response_archive_spools(1).await.unwrap(),
        1
    );
    assert_eq!(budget(&fixture.db).await, 0);
    fixture.finish().await;
}

#[tokio::test]
async fn postgres_gc_skips_a_locked_spool_without_cancelling_its_transaction() {
    let Some(fixture) = PgFixture::new().await else {
        return;
    };
    assert!(
        fixture
            .db
            .begin_response_archive_spool(fixture.id)
            .await
            .unwrap()
    );
    sqlx::query("UPDATE response_archive_spools SET expires_at = 0")
        .execute(&fixture.db.pool)
        .await
        .unwrap();
    let mut blocker = fixture.db.pool.begin().await.unwrap();
    sqlx::query("SELECT request_id FROM response_archive_spools FOR UPDATE")
        .fetch_one(&mut *blocker)
        .await
        .unwrap();

    let cleaned = tokio::time::timeout(
        Duration::from_secs(1),
        fixture
            .db
            .cleanup_response_archive_spools_for(32, Duration::from_millis(100)),
    )
    .await
    .expect("GC must skip a locked spool instead of requiring task cancellation")
    .unwrap();
    assert_eq!(cleaned, 0);

    blocker.rollback().await.unwrap();
    assert_eq!(
        fixture.db.cleanup_response_archive_spools(1).await.unwrap(),
        1
    );
    assert_eq!(budget(&fixture.db).await, 0);
    fixture.finish().await;
}

#[tokio::test]
async fn postgres_batch_reader_obeys_limits_without_waiting_for_global_budget() {
    let Some(fixture) = PgFixture::new().await else {
        return;
    };
    assert!(
        fixture
            .db
            .begin_response_archive_spool(fixture.id)
            .await
            .unwrap()
    );
    // One more than the batch count limit, then three large ciphertext chunks.
    for seq in 0..LOAD_CHUNK_LIMIT + 1 {
        assert!(
            fixture
                .db
                .append_response_archive_spool(fixture.id, seq, 1, "x")
                .await
                .unwrap()
        );
    }
    let large = "x".repeat(CIPHER_CHUNK_LIMIT);
    for seq in LOAD_CHUNK_LIMIT + 1..LOAD_CHUNK_LIMIT + 4 {
        assert!(
            fixture
                .db
                .append_response_archive_spool(fixture.id, seq, 1, &large)
                .await
                .unwrap()
        );
    }
    assert!(
        fixture
            .db
            .seal_response_archive_spool(fixture.id, LOAD_CHUNK_LIMIT + 4, LOAD_CHUNK_LIMIT + 4)
            .await
            .unwrap()
    );
    fixture.terminal().await;
    let task = fixture
        .db
        .claim_response_archive_spool(Uuid::new_v4())
        .await
        .unwrap()
        .unwrap();
    let mut producer = fixture.db.pool.begin().await.unwrap();
    sqlx::query(
        "SELECT cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1 FOR UPDATE",
    )
    .fetch_one(&mut *producer)
    .await
    .unwrap();
    let batch = tokio::time::timeout(
        Duration::from_secs(1),
        fixture.db.load_response_archive_spool_batch(&task, 0),
    )
    .await
    .expect("snapshot batch reader must not acquire the global budget lock")
    .unwrap();
    assert_eq!(batch.len(), LOAD_CHUNK_LIMIT as usize);
    assert_eq!(batch.first().unwrap().seq, 0);
    assert_eq!(batch.last().unwrap().seq, LOAD_CHUNK_LIMIT - 1);
    let large_batch = fixture
        .db
        .load_response_archive_spool_batch(&task, LOAD_CHUNK_LIMIT + 1)
        .await
        .unwrap();
    assert_eq!(large_batch.len(), 2);
    assert_eq!(
        large_batch
            .iter()
            .map(|chunk| chunk.ciphertext.len())
            .sum::<usize>(),
        LOAD_BYTE_LIMIT as usize
    );
    let tail = fixture
        .db
        .load_response_archive_spool_batch(&task, LOAD_CHUNK_LIMIT + 3)
        .await
        .unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].seq, LOAD_CHUNK_LIMIT + 3);
    assert!(
        fixture
            .db
            .load_response_archive_spool_batch(&task, -1)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        fixture
            .db
            .load_response_archive_spool_batch(&task, task.chunk_count)
            .await
            .unwrap()
            .is_empty()
    );
    let mut alien = task.clone();
    alien.identity.tenant_id = Uuid::new_v4();
    assert!(
        fixture
            .db
            .load_response_archive_spool_batch(&alien, 0)
            .await
            .unwrap()
            .is_empty()
    );
    alien = task.clone();
    alien.lease_token = Uuid::new_v4();
    assert!(
        fixture
            .db
            .load_response_archive_spool_batch(&alien, 0)
            .await
            .unwrap()
            .is_empty()
    );
    producer.rollback().await.unwrap();
    sqlx::query("UPDATE response_archive_spools SET lease_expires_at = 0")
        .execute(&fixture.db.pool)
        .await
        .unwrap();
    assert!(
        fixture
            .db
            .load_response_archive_spool_batch(&task, 0)
            .await
            .unwrap()
            .is_empty()
    );
    fixture.finish().await;
}
