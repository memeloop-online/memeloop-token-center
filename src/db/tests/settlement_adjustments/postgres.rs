use super::super::super::*;
use super::{AdjustmentFixture, input};
use uuid::Uuid;

#[tokio::test]
async fn postgres_settlement_adjustments_serialize_versions_and_namespace_caps() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        eprintln!("MTC_TEST_POSTGRES_URL is unset; skipping PostgreSQL settlement adjustment test");
        return;
    };
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let label = format!("settlement-adjustment-postgres-{}", Uuid::now_v7());
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: label.clone(),
                principal_external_id: "member".to_owned(),
                alias: label.clone(),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ZERO,
                idempotency_key: None,
            },
            b"PostgreSQL settlement adjustment fixture pepper over thirty-two bytes",
        )
        .await
        .unwrap();
    database
        .grant(
            issued.account_id,
            Decimal::new(1, 4),
            "adjustment-postgres-fixture",
            &format!("{label}:funding"),
        )
        .await
        .unwrap();
    let settlement_id = Uuid::now_v7();
    let request_id = Uuid::now_v7();
    let now = unix_millis();
    sqlx::query(
        "UPDATE credit_accounts SET available_micros = available_micros - 100 WHERE id = $1 AND available_micros >= 100",
    )
    .bind(issued.account_id.to_string())
    .execute(&database.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO ledger_entries (id, account_id, key_id, kind, amount_micros, currency, source, created_at) VALUES ($1, $2, $3, 'usage', -100, 'USD', $4, $5)",
    )
    .bind(settlement_id.to_string())
    .bind(issued.account_id.to_string())
    .bind(issued.key_id.to_string())
    .bind(&label)
    .bind(now)
    .execute(&database.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO account_settlement_feed (settlement_id, account_id, settlement_sequence, request_id, request_kind, key_id, model, cost_micros, currency, settled_at, completed_at) VALUES ($1, $2, 1, $3, 'text', $4, 'fixture', 100, 'USD', $5, $5)",
    )
    .bind(settlement_id.to_string())
    .bind(issued.account_id.to_string())
    .bind(request_id.to_string())
    .bind(issued.key_id.to_string())
    .bind(now)
    .execute(&database.pool)
    .await
    .unwrap();
    let fixture = AdjustmentFixture {
        _directory: tempfile::tempdir().unwrap(),
        database,
        account_id: issued.account_id,
        settlement_id,
        request_id,
    };
    let same_v1 = input(
        &fixture,
        "memeloop-cloud:usage-discount",
        10,
        1,
        &format!("{label}:same-v1"),
    );
    let same_v2 = input(
        &fixture,
        "memeloop-cloud:usage-discount",
        20,
        2,
        &format!("{label}:same-v2"),
    );
    let (first, second) = tokio::join!(
        fixture.database.reconcile_settlement_adjustment(same_v1),
        fixture.database.reconcile_settlement_adjustment(same_v2),
    );
    assert!(
        second.is_ok(),
        "the newer version must be accepted: {second:?}"
    );
    if let Err(error) = first {
        assert!(
            matches!(error, AppError::Conflict(_)),
            "an older concurrent version may only become stale: {error:?}"
        );
    }
    let state: (i64, i64) = sqlx::query_as(
        "SELECT desired_rebate_micros, version FROM settlement_adjustment_states WHERE account_id = $1 AND settlement_id = $2 AND namespace = 'memeloop-cloud:usage-discount'",
    )
    .bind(fixture.account_id.to_string())
    .bind(fixture.settlement_id.to_string())
    .fetch_one(&fixture.database.pool)
    .await
    .unwrap();
    assert_eq!(state, (20, 2));

    let namespace_one = input(
        &fixture,
        "first:discount",
        60,
        1,
        &format!("{label}:namespace-one"),
    );
    let namespace_two = input(
        &fixture,
        "second:discount",
        60,
        1,
        &format!("{label}:namespace-two"),
    );
    let (first, second) = tokio::join!(
        fixture
            .database
            .reconcile_settlement_adjustment(namespace_one),
        fixture
            .database
            .reconcile_settlement_adjustment(namespace_two),
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    let cumulative: i64 = sqlx::query_scalar(
        "SELECT CAST(COALESCE(SUM(desired_rebate_micros), 0) AS BIGINT) FROM settlement_adjustment_states WHERE account_id = $1 AND settlement_id = $2",
    )
    .bind(fixture.account_id.to_string())
    .bind(fixture.settlement_id.to_string())
    .fetch_one(&fixture.database.pool)
    .await
    .unwrap();
    assert_eq!(cumulative, 80);
    assert!(cumulative <= 100);
}
