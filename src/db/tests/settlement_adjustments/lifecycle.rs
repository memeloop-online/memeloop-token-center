use sqlx::Row;
use uuid::Uuid;

use super::super::super::*;
use super::{fixture, input};

#[tokio::test]
async fn settlement_adjustment_restores_only_entitlement_tail() {
    let fixture = fixture().await;
    let now = unix_millis();
    let entitlement_id = Uuid::now_v7();
    let first_cycle = Uuid::now_v7();
    let second_cycle = Uuid::now_v7();
    let tenant_id: String =
        sqlx::query_scalar("SELECT tenant_id FROM credit_accounts WHERE id = $1")
            .bind(fixture.account_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO subscription_entitlements (id, tenant_id, account_id, provider, external_subscription_id, status, version, current_cycle_id, created_at, updated_at) VALUES ($1, $2, $3, 'test', 'adjustment', 'cancelled', 1, $4, $5, $5)",
    )
    .bind(entitlement_id.to_string())
    .bind(&tenant_id)
    .bind(fixture.account_id.to_string())
    .bind(second_cycle.to_string())
    .bind(now)
    .execute(&fixture.database.pool)
    .await
    .unwrap();
    for (cycle_id, period_end) in [(first_cycle, 10_i64), (second_cycle, 20_i64)] {
        sqlx::query(
            "INSERT INTO entitlement_cycles (id, entitlement_id, external_cycle_id, period_start, period_end, currency, desired_micros, funded_micros, consumed_micros, status, created_at, updated_at) VALUES ($1, $2, $3, 0, $4, 'USD', 50, 50, 50, 'cancelled', $5, $5)",
        )
        .bind(cycle_id.to_string())
        .bind(entitlement_id.to_string())
        .bind(cycle_id.to_string())
        .bind(period_end)
        .bind(now)
        .execute(&fixture.database.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO entitlement_usage_allocations (id, entitlement_cycle_id, usage_ledger_entry_id, amount_micros, created_at) VALUES ($1, $2, $3, 50, $4)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(cycle_id.to_string())
        .bind(fixture.settlement_id.to_string())
        .bind(now)
        .execute(&fixture.database.pool)
        .await
        .unwrap();
    }
    let result = fixture
        .database
        .reconcile_settlement_adjustment(input(
            &fixture,
            "memeloop-cloud:usage-discount",
            60,
            1,
            "adjustment:tail",
        ))
        .await
        .unwrap();
    assert_eq!(result.applied_delta_micros, 60);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT consumed_micros FROM entitlement_cycles WHERE id = $1",
        )
        .bind(first_cycle.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap(),
        40,
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT consumed_micros FROM entitlement_cycles WHERE id = $1",
        )
        .bind(second_cycle.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap(),
        0,
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT available_micros FROM credit_accounts WHERE id = $1")
            .bind(fixture.account_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap(),
        0,
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM entitlement_usage_allocations WHERE usage_ledger_entry_id = $1 AND entitlement_cycle_id = $2",
        )
        .bind(fixture.settlement_id.to_string())
        .bind(second_cycle.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap(),
        0,
    );
    let rollbacks = sqlx::query(
        "SELECT rollback_sequence, entitlement_cycle_id, amount_micros FROM settlement_adjustment_entitlement_rollbacks WHERE event_id = $1 ORDER BY rollback_sequence ASC",
    )
    .bind(result.event_id.to_string())
    .fetch_all(&fixture.database.pool)
    .await
    .unwrap();
    assert_eq!(rollbacks.len(), 2);
    assert_eq!(rollbacks[0].get::<i64, _>("rollback_sequence"), 1);
    assert_eq!(
        rollbacks[0].get::<String, _>("entitlement_cycle_id"),
        second_cycle.to_string(),
    );
    assert_eq!(rollbacks[0].get::<i64, _>("amount_micros"), 50);
    assert_eq!(rollbacks[1].get::<i64, _>("rollback_sequence"), 2);
    assert_eq!(
        rollbacks[1].get::<String, _>("entitlement_cycle_id"),
        first_cycle.to_string(),
    );
    assert_eq!(rollbacks[1].get::<i64, _>("amount_micros"), 10);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(SUM(amount_micros), 0) FROM settlement_adjustment_entitlement_funding_reductions WHERE event_id = $1",
        )
        .bind(result.event_id.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap(),
        60,
        "late cancellation rebates lower cancelled funding instead of creating generic credit",
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM entitlement_cycles WHERE id = $1")
            .bind(second_cycle.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap(),
        "cancelled",
    );
}
