use sqlx::Row;
use uuid::Uuid;

use super::super::super::*;
use super::{fixture, input};

struct LifecycleEntitlement {
    cycle_id: Uuid,
    external_subscription_id: String,
    external_cycle_id: String,
    period_start: i64,
    period_end: i64,
}

async fn establish_consumed_entitlement(
    fixture: &super::AdjustmentFixture,
    suffix: &str,
) -> LifecycleEntitlement {
    let now = unix_millis();
    let external_subscription_id = format!("lifecycle-subscription-{suffix}");
    let external_cycle_id = format!("lifecycle-cycle-{suffix}");
    let period_start = now - 1;
    let period_end = now + 86_400_000;
    let reconciled = fixture
        .database
        .reconcile_entitlement(
            EntitlementOperation::Reconcile(ReconcileEntitlementInput {
                tenant_external_id: "adjustments".to_owned(),
                account_id: fixture.account_id,
                provider: "lifecycle-test".to_owned(),
                external_subscription_id: external_subscription_id.clone(),
                external_cycle_id: external_cycle_id.clone(),
                period_start,
                period_end,
                currency: "USD".to_owned(),
                desired_micros: 100,
                version: 1,
                source: "lifecycle-test".to_owned(),
                proration_json: None,
            }),
            "lifecycle:reconcile",
        )
        .await
        .unwrap();
    // The shared fixture's generic grant is deliberately replaced with the
    // real entitlement grant before attributing the synthetic settled usage.
    fixture
        .database
        .reverse_grant(
            fixture.account_id,
            "adjustment-fixture:funding",
            "lifecycle-replaces-generic-funding",
            "lifecycle:reverse-generic",
        )
        .await
        .unwrap();
    let cycle_id = reconciled.entitlement.cycle_id;
    sqlx::query(
        "UPDATE entitlement_cycles SET consumed_micros = 100 WHERE id = $1 AND funded_micros = 100",
    )
    .bind(cycle_id.to_string())
    .execute(&fixture.database.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO entitlement_usage_allocations (id, entitlement_cycle_id, usage_ledger_entry_id, amount_micros, created_at) VALUES ($1, $2, $3, 100, $4)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(cycle_id.to_string())
    .bind(fixture.settlement_id.to_string())
    .bind(now)
    .execute(&fixture.database.pool)
    .await
    .unwrap();
    LifecycleEntitlement {
        cycle_id,
        external_subscription_id,
        external_cycle_id,
        period_start,
        period_end,
    }
}

async fn cancel_entitlement(
    fixture: &super::AdjustmentFixture,
    entitlement: &LifecycleEntitlement,
) {
    fixture
        .database
        .reconcile_entitlement(
            EntitlementOperation::Cancel(CancelEntitlementInput {
                tenant_external_id: "adjustments".to_owned(),
                provider: "lifecycle-test".to_owned(),
                external_subscription_id: entitlement.external_subscription_id.clone(),
                external_cycle_id: Some(entitlement.external_cycle_id.clone()),
                version: 2,
                source: "lifecycle-test-cancel".to_owned(),
            }),
            "lifecycle:cancel",
        )
        .await
        .unwrap();
}

async fn account_cycle_ledger_snapshot(
    fixture: &super::AdjustmentFixture,
    cycle_id: Uuid,
) -> (i64, i64, i64, i64) {
    let available: i64 =
        sqlx::query_scalar("SELECT available_micros FROM credit_accounts WHERE id = $1")
            .bind(fixture.account_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
    let (funded, consumed): (i64, i64) = sqlx::query_as(
        "SELECT funded_micros, consumed_micros FROM entitlement_cycles WHERE id = $1",
    )
    .bind(cycle_id.to_string())
    .fetch_one(&fixture.database.pool)
    .await
    .unwrap();
    let ledger_total: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount_micros), 0) FROM ledger_entries WHERE account_id = $1",
    )
    .bind(fixture.account_id.to_string())
    .fetch_one(&fixture.database.pool)
    .await
    .unwrap();
    (available, funded, consumed, ledger_total)
}

#[tokio::test]
async fn rebate_then_cancel_matches_cancel_then_late_rebate() {
    let before_cancel = fixture().await;
    let after_cancel = fixture().await;
    let before_entitlement = establish_consumed_entitlement(&before_cancel, "before").await;
    let after_entitlement = establish_consumed_entitlement(&after_cancel, "after").await;

    before_cancel
        .database
        .reconcile_settlement_adjustment(input(
            &before_cancel,
            "memeloop-cloud:usage-discount",
            20,
            1,
            "lifecycle:rebate-before-cancel",
        ))
        .await
        .unwrap();
    cancel_entitlement(&before_cancel, &before_entitlement).await;

    cancel_entitlement(&after_cancel, &after_entitlement).await;
    after_cancel
        .database
        .reconcile_settlement_adjustment(input(
            &after_cancel,
            "memeloop-cloud:usage-discount",
            20,
            1,
            "lifecycle:rebate-after-cancel",
        ))
        .await
        .unwrap();

    let before = account_cycle_ledger_snapshot(&before_cancel, before_entitlement.cycle_id).await;
    let after = account_cycle_ledger_snapshot(&after_cancel, after_entitlement.cycle_id).await;
    assert_eq!(before, after);
    assert_eq!(before, (0, 80, 80, 0));
}

#[tokio::test]
async fn active_desired_reduction_after_consumption_reduces_funding_at_rebate_threshold() {
    let fixture = fixture().await;
    let entitlement = establish_consumed_entitlement(&fixture, "active-reduction").await;
    fixture
        .database
        .reconcile_entitlement(
            EntitlementOperation::Reconcile(ReconcileEntitlementInput {
                tenant_external_id: "adjustments".to_owned(),
                account_id: fixture.account_id,
                provider: "lifecycle-test".to_owned(),
                external_subscription_id: entitlement.external_subscription_id.clone(),
                external_cycle_id: entitlement.external_cycle_id.clone(),
                period_start: entitlement.period_start,
                period_end: entitlement.period_end,
                currency: "USD".to_owned(),
                desired_micros: 40,
                version: 2,
                source: "lifecycle-test-reduce".to_owned(),
                proration_json: None,
            }),
            "lifecycle:reduce-desired",
        )
        .await
        .unwrap();
    let result = fixture
        .database
        .reconcile_settlement_adjustment(input(
            &fixture,
            "memeloop-cloud:usage-discount",
            50,
            1,
            "lifecycle:rebate-after-reduce",
        ))
        .await
        .unwrap();
    assert_eq!(result.applied_delta_micros, 50);
    assert_eq!(
        account_cycle_ledger_snapshot(&fixture, entitlement.cycle_id).await,
        (0, 50, 50, 0)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT amount_micros FROM settlement_adjustment_entitlement_funding_reductions WHERE event_id = $1 AND entitlement_cycle_id = $2",
        )
        .bind(result.event_id.to_string())
        .bind(entitlement.cycle_id.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap(),
        50,
    );
}

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
