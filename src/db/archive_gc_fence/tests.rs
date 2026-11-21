use super::*;

async fn contract(url: &str) {
    let db = Database::connect_with_max(url, 3).await.unwrap();
    db.migrate().await.unwrap();
    let tenant = Uuid::now_v7();
    let legacy = ArchiveGcScope::new(tenant, "legacy").unwrap();
    let training = ArchiveGcScope::new(tenant, &"a".repeat(64)).unwrap();
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let pin = db
        .acquire_archive_gc_pins(
            vec![training.clone(), legacy.clone()],
            token,
            "request",
            owner,
            ArchiveGcPinPurpose::Writer,
        )
        .await
        .unwrap()
        .unwrap();
    let old_epoch = db.archive_gc_root_epoch(&legacy).await.unwrap().unwrap();
    assert!(
        db.claim_archive_gc_exclusive(&legacy, old_epoch, Uuid::now_v7())
            .await
            .unwrap()
            .is_none(),
        "writer remains pinned before publication ACK"
    );
    // Retry does not create another owner and does not erase the existing pin.
    let replay = db
        .acquire_archive_gc_pins(
            vec![legacy.clone(), training.clone()],
            token,
            "request",
            owner,
            ArchiveGcPinPurpose::Writer,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        db.archive_gc_root_epoch(&legacy).await.unwrap(),
        Some(old_epoch)
    );
    assert!(
        db.acquire_archive_gc_pins(
            vec![legacy.clone()],
            token,
            "request",
            Uuid::now_v7(),
            ArchiveGcPinPurpose::Writer
        )
        .await
        .is_err()
    );
    db.release_archive_gc_pins(&replay).await.unwrap();
    db.release_archive_gc_pins(&pin).await.unwrap();
    assert!(
        db.acquire_archive_gc_pins(
            vec![legacy.clone()],
            token,
            "request",
            owner,
            ArchiveGcPinPurpose::Writer
        )
        .await
        .is_err(),
        "released owner cannot return and publish late"
    );
    assert!(
        db.claim_archive_gc_exclusive(&legacy, old_epoch, Uuid::now_v7())
            .await
            .unwrap()
            .is_none(),
        "scan epoch changed during publication"
    );
    let epoch = db.archive_gc_root_epoch(&legacy).await.unwrap().unwrap();
    let exclusive = db
        .claim_archive_gc_exclusive(&legacy, epoch, Uuid::now_v7())
        .await
        .unwrap()
        .unwrap();
    // GC wins first: the same scope cannot acquire writer or reader pins.
    for purpose in [ArchiveGcPinPurpose::Reader, ArchiveGcPinPurpose::Writer] {
        assert!(
            db.acquire_archive_gc_pins(
                vec![legacy.clone()],
                Uuid::now_v7(),
                "request",
                owner,
                purpose
            )
            .await
            .unwrap()
            .is_none()
        );
    }
    // Cross-backend acquisition is atomic; no partial training pin leaks.
    assert!(
        db.acquire_archive_gc_pins(
            vec![training.clone(), legacy.clone()],
            Uuid::now_v7(),
            "request",
            owner,
            ArchiveGcPinPurpose::Writer
        )
        .await
        .unwrap()
        .is_none()
    );
    let training_epoch = db.archive_gc_root_epoch(&training).await.unwrap().unwrap();
    let training_gc = db
        .claim_archive_gc_exclusive(&training, training_epoch, Uuid::now_v7())
        .await
        .unwrap()
        .unwrap();
    assert!(db.finish_archive_gc_exclusive(&training_gc).await.unwrap());
    // A distinct tenant is independent even on the same physical backend.
    let foreign = ArchiveGcScope::new(Uuid::now_v7(), "legacy").unwrap();
    let reader = db
        .acquire_archive_gc_pins(
            vec![foreign.clone()],
            Uuid::now_v7(),
            "request",
            owner,
            ArchiveGcPinPurpose::Reader,
        )
        .await
        .unwrap()
        .unwrap();
    let foreign_epoch = db.archive_gc_root_epoch(&foreign).await.unwrap().unwrap();
    assert!(
        db.claim_archive_gc_exclusive(&foreign, foreign_epoch, Uuid::now_v7())
            .await
            .unwrap()
            .is_none(),
        "active reader protects the stream"
    );
    db.release_archive_gc_pins(&reader).await.unwrap();
    assert!(db.mark_archive_gc_delete_unknown(&exclusive).await.unwrap());
    assert!(
        !db.finish_archive_gc_exclusive(&exclusive).await.unwrap(),
        "unknown cannot be reopened as confirmed"
    );
    db.close().await;
    // Coordination survives process/connection lifetime; there is no TTL expiry.
    let reopened = Database::connect_with_max(url, 2).await.unwrap();
    assert!(
        reopened
            .archive_gc_root_epoch(&legacy)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        reopened
            .acquire_archive_gc_pins(
                vec![legacy],
                Uuid::now_v7(),
                "request",
                owner,
                ArchiveGcPinPurpose::Writer
            )
            .await
            .unwrap()
            .is_none()
    );
    reopened.close().await;
}

#[tokio::test]
async fn sqlite_durable_scope_pin_epoch_and_unknown_delete_contract() {
    let directory = tempfile::tempdir().unwrap();
    contract(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("gc.db").display()
    ))
    .await;
}

#[tokio::test]
async fn postgres_durable_scope_pin_epoch_and_unknown_delete_contract() {
    let url =
        std::env::var("MTC_TEST_POSTGRES_URL").expect("PostgreSQL contract requires real service");
    let isolated = crate::db::test_support::PostgresTestDatabase::create(&url).await;
    contract(isolated.url()).await;
    isolated.cleanup().await;
}
