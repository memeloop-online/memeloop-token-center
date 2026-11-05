use super::*;

async fn chunks(db: &Database) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM response_archive_spool_chunks")
        .fetch_one(&db.pool)
        .await
        .unwrap()
}

async fn expire(db: &Database) {
    sqlx::query("UPDATE response_archive_spools SET expires_at = 0")
        .execute(&db.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn expired_legacy_chunks_and_budget_survive_gc_and_database_reopen() {
    let (dir, db, id) = fixture().await;
    assert!(db.begin_response_archive_spool(id).await.unwrap());
    for seq in 0..130 {
        assert!(
            db.append_response_archive_spool(id, seq, 1, "x")
                .await
                .unwrap()
        );
    }
    expire(&db).await;
    let per_chunk = CHUNK_OVERHEAD + 1;
    assert_eq!(budget(&db).await, SPOOL_OVERHEAD + 130 * per_chunk);
    assert_eq!(db.cleanup_response_archive_spools(1).await.unwrap(), 0);
    assert_eq!(chunks(&db).await, 130);
    assert_eq!(budget(&db).await, SPOOL_OVERHEAD + 130 * per_chunk);
    let row = sqlx::query("SELECT state, cleaned_at, cipher_bytes FROM response_archive_spools")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("state"), "capturing");
    assert!(row.get::<Option<i64>, _>("cleaned_at").is_none());
    assert_eq!(
        row.get::<i64, _>("cipher_bytes"),
        SPOOL_OVERHEAD + 130 * per_chunk
    );
    db.close().await;
    let restarted = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("spool.db").display()
    ))
    .await
    .unwrap();
    assert_eq!(
        restarted.cleanup_response_archive_spools(1).await.unwrap(),
        0
    );
    assert_eq!(chunks(&restarted).await, 130);
    assert_eq!(budget(&restarted).await, SPOOL_OVERHEAD + 130 * per_chunk);
    assert_eq!(
        restarted.cleanup_response_archive_spools(1).await.unwrap(),
        0
    );
    assert_eq!(chunks(&restarted).await, 130);
    assert_eq!(budget(&restarted).await, SPOOL_OVERHEAD + 130 * per_chunk);
    assert_eq!(
        restarted.cleanup_response_archive_spools(1).await.unwrap(),
        0
    );
    let records: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM response_archive_spools WHERE request_id = $1 AND state = 'capturing' AND cleaned_at IS NULL")
        .bind(id.request_id.to_string()).fetch_one(&restarted.pool).await.unwrap();
    assert_eq!(records, 1);
}

#[tokio::test]
async fn expired_large_legacy_chunks_remain_charged_until_verified_migration() {
    let (_dir, db, id) = fixture().await;
    assert!(db.begin_response_archive_spool(id).await.unwrap());
    let ciphertext = "x".repeat(CIPHER_CHUNK_LIMIT);
    for seq in 0..3 {
        assert!(
            db.append_response_archive_spool(id, seq, 1, &ciphertext)
                .await
                .unwrap()
        );
    }
    expire(&db).await;
    let per_chunk = CIPHER_CHUNK_LIMIT as i64 + CHUNK_OVERHEAD;
    assert_eq!(db.cleanup_response_archive_spools(1).await.unwrap(), 0);
    assert_eq!(chunks(&db).await, 3);
    assert_eq!(budget(&db).await, SPOOL_OVERHEAD + 3 * per_chunk);
    assert_eq!(db.cleanup_response_archive_spools(1).await.unwrap(), 0);
    assert_eq!(chunks(&db).await, 3);
    assert_eq!(budget(&db).await, SPOOL_OVERHEAD + 3 * per_chunk);
    assert_eq!(db.cleanup_response_archive_spools(1).await.unwrap(), 0);
    assert_eq!(budget(&db).await, SPOOL_OVERHEAD + 3 * per_chunk);
}

#[tokio::test]
async fn unverified_legacy_gc_never_reaches_chunk_delete_or_either_ledger() {
    let (_dir, db, id) = fixture().await;
    assert!(db.begin_response_archive_spool(id).await.unwrap());
    for seq in 0..65 {
        assert!(
            db.append_response_archive_spool(id, seq, 1, "x")
                .await
                .unwrap()
        );
    }
    expire(&db).await;
    let before = budget(&db).await;
    // Any ledger update would fail. Retention must avoid all mutations,
    // including the earlier chunk deletion, even when expiry has elapsed.
    sqlx::raw_sql("CREATE TRIGGER fail_spool_budget BEFORE UPDATE ON response_archive_spool_budget BEGIN SELECT RAISE(ABORT, 'injected_gc_failure'); END")
        .execute(&db.pool).await.unwrap();
    assert_eq!(db.cleanup_response_archive_spools(1).await.unwrap(), 0);
    assert_eq!(chunks(&db).await, 65);
    assert_eq!(budget(&db).await, before);
    let row = sqlx::query("SELECT cipher_bytes, state, cleaned_at FROM response_archive_spools")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(row.get::<i64, _>("cipher_bytes"), before);
    assert_eq!(row.get::<String, _>("state"), "capturing");
    assert!(row.get::<Option<i64>, _>("cleaned_at").is_none());
    sqlx::query("DROP TRIGGER fail_spool_budget")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(db.cleanup_response_archive_spools(1).await.unwrap(), 0);
    assert_eq!(chunks(&db).await, 65);
    assert_eq!(db.cleanup_response_archive_spools(1).await.unwrap(), 0);
    assert_eq!(budget(&db).await, before);
}
