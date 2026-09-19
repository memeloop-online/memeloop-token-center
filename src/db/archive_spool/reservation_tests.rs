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
    sqlx::query(
        "UPDATE response_archive_spool_budget
         SET cipher_bytes = $1, request_cipher_bytes = $1",
    )
    .bind(REQUEST_CIPHER_LIMIT - amount + 1)
    .execute(&db.pool)
    .await
    .unwrap();
    assert!(
        db.reserve_buffered_archive_capacity(&archive)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(budget(&db).await, REQUEST_CIPHER_LIMIT - amount + 1);
    sqlx::query(
        "UPDATE response_archive_spool_budget
         SET cipher_bytes = $1, request_cipher_bytes = $1",
    )
    .bind(REQUEST_CIPHER_LIMIT - amount)
    .execute(&db.pool)
    .await
    .unwrap();
    let reservation = db
        .reserve_buffered_archive_capacity(&archive)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(budget(&db).await, REQUEST_CIPHER_LIMIT);
    assert!(
        db.reserve_buffered_archive_capacity(&archive)
            .await
            .unwrap()
            .is_none()
    );
    reservation.release().await;
    assert_eq!(budget(&db).await, REQUEST_CIPHER_LIMIT - amount);
}

#[tokio::test]
async fn concurrent_request_reservations_cannot_cross_the_purpose_boundary() {
    let (_directory, db, id) = fixture().await;
    let body = bytes::Bytes::from_static(b"one remaining request archive slot");
    let sample = crate::response_archive_spool::BufferedArchive::new(
        id,
        BufferedArchivePurpose::Request,
        &body,
        PEPPER,
        false,
    )
    .unwrap();
    let amount = SPOOL_OVERHEAD + sample.sealed_len(body.len()).unwrap() as i64 + CHUNK_OVERHEAD;
    sqlx::query(
        "UPDATE response_archive_spool_budget
         SET cipher_bytes = $1, request_cipher_bytes = $1",
    )
    .bind(REQUEST_CIPHER_LIMIT - amount)
    .execute(&db.pool)
    .await
    .unwrap();

    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let db = db.clone();
        let body = body.clone();
        let identity = ArchiveSpoolIdentity {
            request_id: Uuid::new_v4(),
            ..id
        };
        tasks.spawn(async move {
            let archive = crate::response_archive_spool::BufferedArchive::new(
                identity,
                BufferedArchivePurpose::Request,
                &body,
                PEPPER,
                false,
            )
            .unwrap();
            db.reserve_buffered_archive_capacity(&archive)
                .await
                .unwrap()
        });
    }
    let mut reservations = Vec::new();
    while let Some(result) = tasks.join_next().await {
        if let Some(reservation) = result.unwrap() {
            reservations.push(reservation);
        }
    }
    assert_eq!(reservations.len(), 1);
    assert_eq!(budget(&db).await, REQUEST_CIPHER_LIMIT);
    reservations[0].release().await;
    assert_eq!(budget(&db).await, REQUEST_CIPHER_LIMIT - amount);
}

#[tokio::test]
async fn response_pressure_cannot_consume_request_archive_partition() {
    let (_directory, db, id) = fixture().await;
    sqlx::query(
        "UPDATE response_archive_spool_budget
         SET cipher_bytes = $1, request_cipher_bytes = 0",
    )
    .bind(RESPONSE_CIPHER_LIMIT)
    .execute(&db.pool)
    .await
    .unwrap();
    let body = bytes::Bytes::from_static(b"request partition remains available");
    let request_archive = crate::response_archive_spool::BufferedArchive::new(
        id,
        BufferedArchivePurpose::Request,
        &body,
        PEPPER,
        false,
    )
    .unwrap();
    let request_reservation = db
        .reserve_buffered_archive_capacity(&request_archive)
        .await
        .unwrap()
        .expect("response backlog must leave request capacity available");

    let response_archive = crate::response_archive_spool::BufferedArchive::new(
        ArchiveSpoolIdentity {
            request_id: Uuid::new_v4(),
            ..id
        },
        BufferedArchivePurpose::Response,
        &body,
        PEPPER,
        false,
    )
    .unwrap();
    assert!(
        db.reserve_buffered_archive_capacity(&response_archive)
            .await
            .unwrap()
            .is_none()
    );
    request_reservation.release().await;
    assert_eq!(budget(&db).await, RESPONSE_CIPHER_LIMIT);
}

#[tokio::test]
async fn streaming_and_buffered_response_admission_share_one_slot_boundary() {
    let (_directory, db, first_stream) = fixture().await;
    let second_stream = ArchiveSpoolIdentity {
        request_id: Uuid::new_v4(),
        ..first_stream
    };
    sqlx::query(
        "INSERT INTO request_records (
             id, tenant_id, key_id, created_at, protocol, model,
             input_tokens, output_tokens, cost_micros, request_object,
             reservation_id
         )
         SELECT $1, tenant_id, key_id, created_at + 1, protocol, model,
                0, 0, 0, 'inline-json:{}', reservation_id
         FROM request_records WHERE id = $2",
    )
    .bind(second_stream.request_id.to_string())
    .bind(first_stream.request_id.to_string())
    .execute(&db.pool)
    .await
    .unwrap();

    sqlx::query(
        "WITH digits(d) AS (
             VALUES (0), (1), (2), (3), (4), (5), (6), (7), (8), (9)
         ), numbered(n) AS (
             SELECT ones.d + 10 * tens.d + 100 * hundreds.d + 1000 * thousands.d
             FROM digits ones
             CROSS JOIN digits tens
             CROSS JOIN digits hundreds
             CROSS JOIN digits thousands
         )
         INSERT INTO archive_budget_reservations
             (id, request_id, purpose, cipher_bytes, expires_at)
         SELECT printf('response-slot-%04d', n), $1, 'response', 0, 9223372036854775807
         FROM numbered WHERE n < $2",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(ARCHIVE_SLOT_LIMIT - 2)
    .execute(&db.pool)
    .await
    .unwrap();

    let body = bytes::Bytes::from_static(b"buffered response slot");
    let buffered = crate::response_archive_spool::BufferedArchive::new(
        ArchiveSpoolIdentity {
            request_id: Uuid::new_v4(),
            ..first_stream
        },
        BufferedArchivePurpose::Response,
        &body,
        PEPPER,
        false,
    )
    .unwrap();
    let reservation = db
        .reserve_buffered_archive_capacity(&buffered)
        .await
        .unwrap()
        .expect("the last two response slots must admit one buffered reservation");
    assert!(db.begin_response_archive_spool(first_stream).await.unwrap());

    let active_slots: i64 = sqlx::query_scalar(
        "SELECT
            (SELECT COUNT(*) FROM response_archive_spools
             WHERE cleaned_at IS NULL AND state IN ('capturing', 'pending', 'uploading'))
          + (SELECT COUNT(*) FROM archive_budget_reservations
             WHERE purpose = 'response')",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(active_slots, ARCHIVE_SLOT_LIMIT);
    assert!(
        !db.begin_response_archive_spool(second_stream)
            .await
            .unwrap()
    );
    reservation.release().await;
}
