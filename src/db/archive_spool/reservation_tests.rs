use super::*;

const PEPPER: &[u8] = b"archive-reservation-contract-pepper-over-32-bytes";

#[tokio::test]
async fn reserved_capture_preserves_exact_accounting_through_commit_recovery_and_legacy_gc() {
    let (_directory, db, id) = fixture().await;
    let body = bytes::Bytes::from(vec![b'x'; 128 * 1024]);
    let archive = crate::response_archive_spool::BufferedArchive::new(
        id,
        BufferedArchivePurpose::Request,
        &body,
        PEPPER,
        true,
    )
    .unwrap();
    let reservation = db
        .reserve_buffered_archive_capacity(&archive)
        .await
        .unwrap()
        .unwrap();
    let reserved = budget(&db).await;
    let (mut tx, now, hold) = db
        .reserved_spool_transaction(&reservation, "test_reserved_capture")
        .await
        .unwrap();
    assert!(
        db.capture_reserved_buffered_archive_body_in_transaction(
            &mut tx,
            now,
            &archive,
            None,
            Some(&reservation),
        )
        .await
        .unwrap()
    );
    hold.commit(tx).await.unwrap();
    let actual: i64 = sqlx::query_scalar("SELECT cipher_bytes FROM request_archive_spools")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let unused: i64 = sqlx::query_scalar("SELECT cipher_bytes FROM archive_budget_reservations")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(
        actual < reserved,
        "compression must leave refundable capacity"
    );
    assert_eq!(budget(&db).await, actual + unused);

    // Model a process disappearing after COMMIT but before refund. The durable
    // row alone is enough to recover; committed spool bytes stay charged.
    sqlx::query("UPDATE archive_budget_reservations SET expires_at = 0")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        db.cleanup_expired_archive_budget_reservation()
            .await
            .unwrap()
    );
    assert!(
        !db.cleanup_expired_archive_budget_reservation()
            .await
            .unwrap()
    );
    assert_eq!(budget(&db).await, actual);
    assert!(
        db.reserved_spool_transaction(&reservation, "expired")
            .await
            .is_err()
    );
    reservation.release().await;
    reservation.release().await;
    assert_eq!(
        budget(&db).await,
        actual,
        "late/refund retry cannot double decrement"
    );
    let request_budget: i64 =
        sqlx::query_scalar("SELECT request_cipher_bytes FROM response_archive_spool_budget")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(request_budget, actual);
    sqlx::query("UPDATE request_archive_spools SET expires_at = 0")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(db.cleanup_response_archive_spools(32).await.unwrap(), 1);
    assert_eq!(budget(&db).await, 0);
}

#[tokio::test]
async fn reserved_capture_rollback_restores_credit_and_release_refunds_only_once() {
    let (_directory, db, id) = fixture().await;
    let body = bytes::Bytes::from_static(b"rolled back capture");
    let archive = crate::response_archive_spool::BufferedArchive::new(
        id,
        BufferedArchivePurpose::Response,
        &body,
        PEPPER,
        false,
    )
    .unwrap();
    let reservation = db
        .reserve_buffered_archive_capacity(&archive)
        .await
        .unwrap()
        .unwrap();
    let reserved = budget(&db).await;
    let (mut tx, now, _) = db
        .reserved_spool_transaction(&reservation, "test_rollback")
        .await
        .unwrap();
    assert!(
        db.capture_reserved_buffered_archive_body_in_transaction(
            &mut tx,
            now,
            &archive,
            None,
            Some(&reservation),
        )
        .await
        .unwrap()
    );
    tx.rollback().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM response_archive_spools")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT cipher_bytes FROM archive_budget_reservations")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        reserved
    );
    assert_eq!(budget(&db).await, reserved);
    reservation.release().await;
    reservation.release().await;
    assert_eq!(budget(&db).await, 0);
}

#[tokio::test]
async fn reservations_preserve_the_global_capacity_boundary() {
    let (_directory, db, id) = fixture().await;
    let body = bytes::Bytes::from_static(b"capacity boundary");
    let archive = crate::response_archive_spool::BufferedArchive::new(
        id,
        BufferedArchivePurpose::Request,
        &body,
        PEPPER,
        false,
    )
    .unwrap();
    let amount = SPOOL_OVERHEAD + archive.sealed_len(body.len()).unwrap() as i64 + CHUNK_OVERHEAD;
    sqlx::query("UPDATE response_archive_spool_budget SET cipher_bytes = $1")
        .bind(CIPHER_LIMIT - amount + 1)
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        db.reserve_buffered_archive_capacity(&archive)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(budget(&db).await, CIPHER_LIMIT - amount + 1);
    sqlx::query("UPDATE response_archive_spool_budget SET cipher_bytes = $1")
        .bind(CIPHER_LIMIT - amount)
        .execute(&db.pool)
        .await
        .unwrap();
    let reservation = db
        .reserve_buffered_archive_capacity(&archive)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(budget(&db).await, CIPHER_LIMIT);
    assert!(
        db.reserve_buffered_archive_capacity(&archive)
            .await
            .unwrap()
            .is_none()
    );
    reservation.release().await;
    assert_eq!(budget(&db).await, CIPHER_LIMIT - amount);
}
