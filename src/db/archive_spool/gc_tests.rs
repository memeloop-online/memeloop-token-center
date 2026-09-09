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
async fn tiny_chunks_gc_resumes_after_database_reopen_without_releasing_live_budget() {
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
    assert_eq!(chunks(&db).await, 66);
    assert_eq!(budget(&db).await, SPOOL_OVERHEAD + 66 * per_chunk);
    let row = sqlx::query("SELECT state, cleaned_at, cipher_bytes FROM response_archive_spools")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("state"), "gap");
    assert!(row.get::<Option<i64>, _>("cleaned_at").is_none());
    assert_eq!(
        row.get::<i64, _>("cipher_bytes"),
        SPOOL_OVERHEAD + 66 * per_chunk
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
    assert_eq!(chunks(&restarted).await, 2);
    assert_eq!(budget(&restarted).await, SPOOL_OVERHEAD + 2 * per_chunk);
    assert_eq!(
        restarted.cleanup_response_archive_spools(1).await.unwrap(),
        1
    );
    assert_eq!(chunks(&restarted).await, 0);
    assert_eq!(budget(&restarted).await, 0);
    assert_eq!(
        restarted.cleanup_response_archive_spools(1).await.unwrap(),
        0
    );
    assert!(!restarted.begin_response_archive_spool(id).await.unwrap());
}

#[tokio::test]
async fn gc_byte_budget_wins_before_chunk_limit() {
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
    // Two maximum ciphertext chunks plus row overhead exceed 1MiB.
    assert_eq!(chunks(&db).await, 2);
    assert_eq!(budget(&db).await, SPOOL_OVERHEAD + 2 * per_chunk);
    assert_eq!(db.cleanup_response_archive_spools(1).await.unwrap(), 0);
    assert_eq!(chunks(&db).await, 1);
    assert_eq!(budget(&db).await, SPOOL_OVERHEAD + per_chunk);
    assert_eq!(db.cleanup_response_archive_spools(1).await.unwrap(), 1);
    assert_eq!(budget(&db).await, 0);
}

#[tokio::test]
async fn gc_failure_after_chunk_delete_rolls_back_chunks_and_both_ledgers() {
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
    // Fail the final singleton update, after DELETE and spool ledger changes.
    sqlx::raw_sql("CREATE TRIGGER fail_spool_budget BEFORE UPDATE ON response_archive_spool_budget BEGIN SELECT RAISE(ABORT, 'injected_gc_failure'); END")
        .execute(&db.pool).await.unwrap();
    assert!(db.cleanup_response_archive_spools(1).await.is_err());
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
    assert_eq!(chunks(&db).await, 1);
    assert_eq!(db.cleanup_response_archive_spools(1).await.unwrap(), 1);
    assert_eq!(budget(&db).await, 0);
}
