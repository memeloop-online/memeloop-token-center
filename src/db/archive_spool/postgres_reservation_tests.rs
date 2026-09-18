use super::*;

#[tokio::test]
async fn postgres_concurrent_streams_progress_while_buffered_capture_transaction_is_open() {
    let Some(fixture) = PgFixture::new().await else {
        return;
    };
    let body = bytes::Bytes::from(vec![b'x'; 128 * 1024]);
    let archive = crate::response_archive_spool::BufferedArchive::new(
        fixture.id,
        BufferedArchivePurpose::Request,
        &body,
        b"postgres-capacity-contract-pepper-over-32-bytes",
        true,
    )
    .unwrap();
    let capacity = fixture
        .db
        .reserve_buffered_archive_capacity(&archive)
        .await
        .unwrap()
        .unwrap();
    let reserved = budget(&fixture.db).await;
    let (mut held, now, hold) = fixture
        .db
        .reserved_spool_transaction(&capacity, "paused_capture")
        .await
        .unwrap();
    assert!(
        fixture
            .db
            .capture_reserved_buffered_archive_body_in_transaction(
                &mut held,
                now,
                &archive,
                None,
                Some(&capacity),
            )
            .await
            .unwrap()
    );

    // Keep the buffered transaction open, representing compression/account or
    // session contention. 32 streams use a five-connection pool; none requires
    // releasing this transaction, and each commits eight independent appends.
    let mut streams = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let db = fixture.db.clone();
        let identity = ArchiveSpoolIdentity {
            request_id: Uuid::new_v4(),
            ..fixture.id
        };
        streams.spawn(async move {
            sqlx::query(
                "INSERT INTO request_records (id, tenant_id, reservation_id) VALUES ($1, $2, $3)",
            )
            .bind(identity.request_id.to_string())
            .bind(identity.tenant_id.to_string())
            .bind(identity.reservation_id.to_string())
            .execute(&db.pool)
            .await
            .unwrap();
            assert!(db.begin_response_archive_spool(identity).await.unwrap());
            for seq in 0..8 {
                assert!(
                    db.append_response_archive_spool(identity, seq, 1, "x")
                        .await
                        .unwrap()
                );
            }
        });
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(result) = streams.join_next().await {
            result.unwrap();
        }
    })
    .await
    .expect("stream appends must not queue behind unrelated buffered lifecycle work");
    let streamed = 32 * (SPOOL_OVERHEAD + 8 * (CHUNK_OVERHEAD + 1));
    assert_eq!(budget(&fixture.db).await, reserved + streamed);
    hold.commit(held).await.unwrap();
    capacity.release().await;
    let actual: i64 = sqlx::query_scalar("SELECT cipher_bytes FROM request_archive_spools")
        .fetch_one(&fixture.db.pool)
        .await
        .unwrap();
    assert_eq!(budget(&fixture.db).await, actual + streamed);
    drop(capacity);
    fixture.finish().await;
}

#[tokio::test]
async fn postgres_expired_reservation_recovery_skips_a_live_owner_and_is_exact() {
    let Some(fixture) = PgFixture::new().await else {
        return;
    };
    let body = bytes::Bytes::from_static(b"expired reservation");
    let archive = crate::response_archive_spool::BufferedArchive::new(
        fixture.id,
        BufferedArchivePurpose::Request,
        &body,
        b"postgres-capacity-contract-pepper-over-32-bytes",
        false,
    )
    .unwrap();
    let first = fixture
        .db
        .reserve_buffered_archive_capacity(&archive)
        .await
        .unwrap()
        .unwrap();
    let second = fixture
        .db
        .reserve_buffered_archive_capacity(&archive)
        .await
        .unwrap()
        .unwrap();
    let total = budget(&fixture.db).await;
    sqlx::query("UPDATE archive_budget_reservations SET expires_at = 0")
        .execute(&fixture.db.pool)
        .await
        .unwrap();
    let mut owner = fixture.db.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM archive_budget_reservations ORDER BY id LIMIT 1 FOR UPDATE")
        .fetch_one(&mut *owner)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(2),
            fixture.db.cleanup_expired_archive_budget_reservation()
        )
        .await
        .expect("expiry collection must skip a live private reservation lock")
        .unwrap()
    );
    assert_eq!(budget(&fixture.db).await, total / 2);
    assert!(
        !fixture
            .db
            .cleanup_expired_archive_budget_reservation()
            .await
            .unwrap()
    );
    owner.rollback().await.unwrap();
    assert!(
        fixture
            .db
            .cleanup_expired_archive_budget_reservation()
            .await
            .unwrap()
    );
    first.release().await;
    second.release().await;
    assert_eq!(budget(&fixture.db).await, 0);
    drop((first, second));
    fixture.finish().await;
}
