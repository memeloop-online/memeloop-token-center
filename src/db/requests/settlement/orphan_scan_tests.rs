use super::*;
use rust_decimal::Decimal;

async fn bounded_scan_contract(database: &Database) {
    let pepper = b"bounded orphan reservation scan contract pepper";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: Uuid::now_v7().to_string(),
                principal_external_id: "member".into(),
                alias: "bounded-orphans".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::from(10),
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let key = database
        .authenticate_key(&issued.key, pepper)
        .await
        .unwrap();
    let price = database
        .upsert_model_price("bounded-orphan", "USD", Decimal::ZERO, Decimal::ONE)
        .await
        .unwrap();
    let old = unix_millis() - 60 * 60 * 1000;
    let mut reservations = Vec::new();
    let mut requests = Vec::new();
    for ordinal in 0..3_i64 {
        let reservation = database.reserve_usage(&key, &price, 0, 100).await.unwrap();
        sqlx::query("UPDATE usage_reservations SET created_at = $1 WHERE id = $2")
            .bind(old + ordinal)
            .bind(reservation.id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        if ordinal < 2 {
            let request_id = Uuid::now_v7();
            database
                .record_request_started(NewRequest {
                    request_id,
                    key_id: key.key_id,
                    tenant_id: key.tenant_id,
                    protocol: "openai".into(),
                    model: "bounded-orphan".into(),
                    request_object: format!("gap://{request_id}/request"),
                    reservation_id: reservation.id,
                    upstream_account_id: None,
                    model_route_id: None,
                })
                .await
                .unwrap();
            // Deliberately different timestamps: qualification must use the
            // reservation link, never an invented equality/time window.
            if ordinal == 0 {
                sqlx::query("UPDATE request_records SET completed_at = $1 WHERE id = $2")
                    .bind(unix_millis())
                    .bind(request_id.to_string())
                    .execute(&database.pool)
                    .await
                    .unwrap();
            } else {
                sqlx::query("UPDATE request_records SET submission_started_at = $1 WHERE id = $2")
                    .bind(unix_millis())
                    .bind(request_id.to_string())
                    .execute(&database.pool)
                    .await
                    .unwrap();
            }
            requests.push(request_id);
        }
        reservations.push(reservation);
    }
    let mut scan = OrphanReservationScan::default();
    for (ordinal, reservation) in reservations.iter().enumerate() {
        let released = database
            .release_orphaned_reservations_page(1, &mut scan)
            .await
            .unwrap();
        assert_eq!(released, u64::from(ordinal == 2));
        assert_eq!(
            scan.reservation_after,
            Some((old + ordinal as i64, reservation.id.to_string()))
        );
    }
    for reservation in &reservations[..2] {
        let status: String =
            sqlx::query_scalar("SELECT status FROM usage_reservations WHERE id = $1")
                .bind(reservation.id.to_string())
                .fetch_one(&database.pool)
                .await
                .unwrap();
        assert_eq!(status, "reserved");
    }
    let actual: i64 =
        sqlx::query_scalar("SELECT actual_micros FROM usage_reservations WHERE id = $1")
            .bind(reservations[2].id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(actual, 0);
    // A previously ineligible row becomes eligible behind the cursor. A full
    // pass resets it and rechecks the row without changing any money twice.
    sqlx::query("UPDATE request_records SET completed_at = NULL WHERE id = $1")
        .bind(requests[0].to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    assert_eq!(
        database
            .release_orphaned_reservations_page(1, &mut scan)
            .await
            .unwrap(),
        0
    );
    assert!(scan.reservation_after.is_none());
    assert_eq!(
        database
            .release_orphaned_reservations_page(1, &mut scan)
            .await
            .unwrap(),
        1
    );
    let ledger_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ledger_entries WHERE kind = 'usage' AND source = $1",
    )
    .bind(reservations[0].id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(ledger_count, 1);
}

#[tokio::test]
async fn sqlite_orphan_scan_advances_past_ineligible_rows_and_revisits_late_eligibility() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("orphan.db").display()
    ))
    .await
    .unwrap();
    database.migrate().await.unwrap();
    bounded_scan_contract(&database).await;
}

#[tokio::test]
async fn postgres_orphan_scan_advances_past_ineligible_rows_and_revisits_late_eligibility() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        assert!(
            std::env::var_os("CI").is_none(),
            "PostgreSQL orphan scan contract requires MTC_TEST_POSTGRES_URL"
        );
        return;
    };
    sqlx::any::install_default_drivers();
    let admin = sqlx::AnyPool::connect(&database_url).await.unwrap();
    let schema = format!("orphan_scan_{}", Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut url = url::Url::parse(&database_url).unwrap();
    url.query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let database = Database::connect(url.as_str()).await.unwrap();
    database.migrate().await.unwrap();
    bounded_scan_contract(&database).await;
    database.pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
